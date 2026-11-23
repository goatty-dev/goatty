//! The welcome window: goatty's first-run setup (#40). Three short steps,
//! only what matters on day one; everything else has good defaults and
//! lives in the settings window:
//!
//! 1. **Look**: a theme (or follow the desktop's light/dark) and the text size.
//! 2. **Shell**: goatty's built-in tools on the window's host, preselected
//!    when missing, installed with progress (no sudo, pinned checksums),
//!    whether to import the PATH folders and variables the user's own
//!    `.zshrc`/`.bashrc` sets up (on by default; shown when they have one,
//!    with what it brings), whether to use goatty's shell in every terminal
//!    (off by default; the exact line and file are shown, so the switch is
//!    the consent), and the prompt style.
//! 3. **Ready**: the few shortcuts worth knowing, links onward.
//!
//! It opens by itself once, on the very first start (no client settings
//! file and no "shown" marker in goatty's state directory), and again from
//! the command palette ("Welcome and setup"). While it's open, a tab
//! connecting doesn't show the install toast (`crate::shell_setup`): this
//! window asks instead.

use std::collections::BTreeSet;

use goatty_config::{HostSettings, PromptStyle, ThemeEntry, store};
use goatty_shell::setup::{self, Status, ToolLocation};
use gpui::{
    App, AppContext, Bounds, Context, FocusHandle, Focusable, FontWeight, Global, IntoElement, KeyBinding, ParentElement, Render,
    SharedString, Styled, Subscription, Window, WindowBounds, WindowHandle, WindowOptions, actions, div, prelude::*, px, rgb, size,
};

use crate::settings::AppSettings;
// The settings window's building blocks, so this window looks the same.
use crate::settings_window::kit::{choice_card as choice, row as settings_row, section as settings_section};
use crate::settings_window::{self, HostLink, HostLinkGlobal, Page};
use crate::shell_setup::{self, Progress};
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button, button_with, keycaps, stepper, toggle};
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};

const CONTEXT: &str = "Welcome";

actions!(welcome, [CloseWelcome]);

pub fn bindings() -> Vec<KeyBinding> {
    vec![KeyBinding::new("escape", CloseWelcome, Some(CONTEXT)), KeyBinding::new("ctrl-w", CloseWelcome, Some(CONTEXT))]
}

struct OpenWindow(Option<WindowHandle<Welcome>>);

impl Global for OpenWindow {}

/// Set between deciding this is the first run and opening the window, so
/// the terminal window's tab (which connects first) doesn't offer the
/// setup in a toast as well.
struct FirstRun;

impl Global for FirstRun {}

/// The welcome window is open (or about to open on the first run).
pub fn is_open(cx: &App) -> bool {
    cx.has_global::<FirstRun>() || cx.try_global::<OpenWindow>().and_then(|w| w.0).is_some_and(|h| h.read(cx).is_ok())
}

/// Before the first terminal window opens: is this the very first start?
/// Then [`open_if_first_run`] opens the welcome window. Someone who already
/// has a settings file isn't new; either way it's decided only once (a
/// marker in goatty's state directory).
pub fn check_first_run(cx: &mut App) {
    let Some(marker) = goatty_config::welcome_marker() else { return };
    if marker.exists() {
        return;
    }
    let new = goatty_config::client_file().is_none_or(|f| !f.exists());
    if let Err(e) = marker.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&marker, "")) {
        // Without the marker it would open on every start: don't open it.
        log::warn!("couldn't write {}: {e}; not showing the welcome window", marker.display());
        return;
    }
    if new {
        cx.set_global(FirstRun);
    }
}

/// After the terminal window opened: show the welcome window on the first run.
pub fn open_if_first_run(cx: &mut App) {
    if cx.has_global::<FirstRun>() {
        open(cx);
        cx.remove_global::<FirstRun>();
    }
}

/// Open the welcome window at its first step, or bring it to the front.
pub fn open(cx: &mut App) {
    if let Some(h) = cx.try_global::<OpenWindow>().and_then(|w| w.0)
        && h.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return;
    }
    let bounds = Bounds::centered(None, size(px(780.), px(660.)), cx);
    let opened = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(640.), px(540.))),
            app_id: Some("dev.goatty.goatty".into()),
            ..Default::default()
        },
        |window, cx| {
            window.set_window_title("Welcome to goatty");
            cx.new(|cx| Welcome::new(window, cx))
        },
    );
    match opened {
        Ok(h) => cx.set_global(OpenWindow(Some(h))),
        Err(e) => log::error!("couldn't open the welcome window: {e:#}"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Look,
    Shell,
    Ready,
}

impl Step {
    const ALL: [Step; 3] = [Step::Look, Step::Shell, Step::Ready];

    fn label(self) -> &'static str {
        match self {
            Step::Look => "Look",
            Step::Shell => "Shell",
            Step::Ready => "Ready",
        }
    }
}

pub struct Welcome {
    focus: FocusHandle,
    step: Step,
    /// The host whose shell step 2 sets up (the terminal window's).
    link: Option<HostLink>,
    status: Option<Status>,
    loading: bool,
    error: Option<String>,
    /// Tools to install (ids); starts as what's missing.
    chosen: BTreeSet<String>,
    progress: Option<Progress>,
    /// The host's settings, for the prompt style.
    host: Option<HostSettings>,
    /// Saving the host settings this window changes (prompt style, import),
    /// one save at a time; `save_again`: changed meanwhile, save once more.
    saving: bool,
    save_again: bool,
    /// The values the host has (loaded, or saved last), to go back to when
    /// a save fails.
    committed: Option<(PromptStyle, bool)>,
    prompt_error: Option<String>,
    /// What importing the user's shell setup would bring (`None`: not asked yet).
    import: Option<goatty_shell::import::Status>,
    /// The host runs the user's shell to see what it sets up.
    import_checking: bool,
    import_error: Option<String>,
    /// goatty's shell in every terminal: the line in the host's `.zshenv`.
    everywhere: Option<goatty_shell::everywhere::Status>,
    everywhere_busy: bool,
    everywhere_error: Option<String>,
    _subs: Vec<Subscription>,
}

impl Focusable for Welcome {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Welcome {
    fn new(window: &mut Window, cx: &mut Context<'_, Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus, cx);
        let subs = vec![
            cx.observe_global::<AppSettings>(|_, cx| cx.notify()),
            // The terminal window connects after this opens: pick its host up.
            cx.observe_global::<HostLinkGlobal>(|this, cx| {
                if this.link.is_none() {
                    this.link = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone());
                    if this.link.is_some() && this.step == Step::Shell {
                        this.load(cx);
                    }
                    cx.notify();
                }
            }),
        ];
        Self {
            focus,
            step: Step::Look,
            link: cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone()),
            status: None,
            loading: false,
            error: None,
            chosen: BTreeSet::new(),
            progress: None,
            host: None,
            saving: false,
            save_again: false,
            committed: None,
            prompt_error: None,
            import: None,
            import_checking: false,
            import_error: None,
            everywhere: None,
            everywhere_busy: false,
            everywhere_error: None,
            _subs: subs,
        }
    }

    fn close(&mut self, _: &CloseWelcome, window: &mut Window, _cx: &mut Context<'_, Self>) {
        window.remove_window();
    }

    fn go(&mut self, step: Step, cx: &mut Context<'_, Self>) {
        self.step = step;
        if step == Step::Shell && self.status.is_none() && !self.loading {
            self.load(cx);
        }
        cx.notify();
    }

    /// Ask the host what its shell has (and its settings, for the prompt).
    fn load(&mut self, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        self.loading = true;
        self.error = None;
        // This window asks: no toast for the same host later in this run.
        shell_setup::mark_asked(&link, cx);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let status = link.shell_status()?;
                    if status.should_offer() && !status.offered {
                        // The hint in new shells stays quiet: this window asks.
                        if let Err(e) = link.shell(setup::Request::Offered) {
                            log::warn!("couldn't tell the host the setup was offered: {e:#}");
                        }
                    }
                    let host = link.get().ok().map(|s| store::parse::<HostSettings>(&s.json).value);
                    anyhow::Ok((status, host))
                })
                .await;
            let _ = this.update(cx, |v, cx| {
                v.loading = false;
                match result {
                    Ok((status, host)) => {
                        v.chosen = status.offer().chain(status.updates()).map(|t| t.id.clone()).collect();
                        let managed = status.managed;
                        v.status = Some(status);
                        v.committed = host.as_ref().map(|h| (h.prompt.style, h.shell.import.enabled));
                        v.host = host;
                        if managed {
                            v.load_import(cx);
                            v.everywhere(goatty_shell::everywhere::Action::Status, cx);
                        }
                    }
                    Err(e) => v.error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// What the user's own startup files set up: asked first, then (when
    /// nothing was checked yet) the host runs their shell once to find out.
    fn load_import(&mut self, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        self.import_checking = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let first = link.clone();
            let status = cx.background_executor().spawn(async move { first.import_status(false) }).await;
            let check = matches!(&status, Ok(s) if s.harvest.is_none() && s.source.is_some() && s.files.iter().any(|f| f.exists));
            let _ = this.update(cx, |v, cx| {
                v.import = status.ok();
                v.import_checking = check;
                cx.notify();
            });
            if check {
                let status = cx.background_executor().spawn(async move { link.import_status(true) }).await;
                let _ = this.update(cx, |v, cx| {
                    if let Ok(mut s) = status {
                        // A choice made while the check ran is newer than what it read.
                        if let Some(old) = &v.import {
                            s.enabled = old.enabled;
                        }
                        v.import = Some(s);
                    }
                    v.import_checking = false;
                    cx.notify();
                });
            }
        })
        .detach();
    }

    /// Ask the host about the line in its `.zshenv`, or add or remove it.
    fn everywhere(&mut self, action: goatty_shell::everywhere::Action, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else { return };
        if self.everywhere_busy {
            return;
        }
        self.everywhere_busy = true;
        self.everywhere_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx.background_executor().spawn(async move { link.everywhere(action) }).await;
            let _ = this.update(cx, |v, cx| {
                v.everywhere_busy = false;
                match result {
                    Ok(s) => v.everywhere = Some(s),
                    // An older goattyd: no question about it.
                    Err(e) if action == goatty_shell::everywhere::Action::Status => log::info!("every terminal: {e:#}"),
                    Err(e) => v.everywhere_error = Some(format!("{e:#}")),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// "Use goatty's shell in every terminal", with the exact lines and
    /// files shown with the switch: turning it on is the consent.
    fn everywhere_section(&self, ui: Ui, cx: &Context<'_, Self>) -> Option<impl IntoElement> {
        use goatty_shell::everywhere::{Action, State};
        use settings_window::everywhere::{LABEL, code_lines, warning_line};
        let s = self.everywhere.as_ref()?;
        let on = s.state == State::On;
        let (desc, can) = match (&s.state, &s.unavailable) {
            (State::On, _) => (format!("{} starts goatty's shell in every new interactive zsh", s.files_text()), true),
            (State::Changed, _) => {
                let at = s.changed.first().map_or_else(|| s.files_text(), ToString::to_string);
                (format!("A goatty line was edited by hand ({at}). Edit or remove it there"), false)
            }
            (State::Off, Some(why)) => (why.clone(), false),
            (State::Off, None) => ("VS Code and other editors, other terminals, tmux and SSH logins get this shell too".to_owned(), true),
        };
        let busy = self.everywhere_busy;
        let mut below = div().flex().flex_col().gap(px(6.));
        // The lines only where the switch can add or has added them.
        if can {
            below = below
                .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child(SharedString::from(format!(
                    "{} {}:",
                    if on { "goatty added these lines to the end of" } else { "Turning it on adds these lines to the end of" },
                    s.files_text()
                ))))
                .child(code_lines(ui, "everywhere-lines", crate_mono(cx), &s.lines));
        }
        below = below.children(s.warnings.iter().map(|w| warning_line(ui, w.clone())));
        if let Some(e) = &self.everywhere_error {
            below = below.child(banner(ui, Tone::Danger, "Couldn't change it", e.lines().map(str::to_owned).collect()));
        }
        let sec = settings_section(ui, "Every terminal").child(
            settings_row(ui, LABEL)
                .desc(desc)
                .disabled(!can || busy)
                .control(
                    toggle("everywhere", on, ui, self.act(cx, move |v, cx| v.everywhere(if on { Action::Off } else { Action::On }, cx)))
                        .aria_label(LABEL),
                )
                .below(below),
        );
        let icons = self.host.as_ref().is_some_and(|h| h.prompt.plain_elsewhere());
        Some(sec.footer(if icons {
            "Your ~/.zshrc won't run there (its PATH folders and variables still come along), and prompt icons fall back to text. Change it any time under Settings → Shell."
        } else {
            "Your ~/.zshrc won't run there (its PATH folders and variables still come along). Change it any time under Settings → Shell."
        }))
    }

    /// Turn the import on or off on the host.
    fn set_import(&mut self, on: bool, cx: &mut Context<'_, Self>) {
        let Some(s) = self.import.as_mut() else { return };
        s.enabled = on;
        self.save(cx);
    }

    /// Save the prompt style and the import choice shown here to the host
    /// (a new generation; running shells reload). Both go in one
    /// read-modify-write, one save at a time, so quick changes to either
    /// don't undo each other.
    fn save(&mut self, cx: &mut Context<'_, Self>) {
        if self.saving {
            self.save_again = true;
            return;
        }
        let Some(link) = self.link.clone() else { return };
        let style = self.host.as_ref().map(|h| h.prompt.style);
        let import = self.import.as_ref().map(|s| s.enabled);
        self.saving = true;
        self.prompt_error = None;
        self.import_error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    // Edit what the host has now, keeping keys this build doesn't know.
                    let loaded = store::parse::<HostSettings>(&link.get()?.json);
                    let mut value = loaded.value;
                    if let Some(style) = style {
                        value.prompt.style = style;
                    }
                    if let Some(on) = import {
                        value.shell.import.enabled = on;
                    }
                    let committed = (value.prompt.style, value.shell.import.enabled);
                    let applied = link.set(store::to_json(&value, &loaded.unknown), true)?;
                    // Saved: a shell reload that fails doesn't undo it.
                    let reload = if applied.applied && !applied.diff.is_empty() { link.reload_shells().err() } else { None };
                    anyhow::Ok((committed, reload))
                })
                .await;
            let _ = this.update(cx, |v, cx| {
                v.saving = false;
                match result {
                    Ok((committed, reload)) => {
                        v.committed = Some(committed);
                        if let Some(e) = reload {
                            let e = format!("Saved, but running shells didn't reload: {e:#}");
                            v.prompt_error = Some(e.clone());
                            v.import_error = Some(e);
                        }
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        v.prompt_error = Some(e.clone());
                        v.import_error = Some(e);
                        // Show what the host has.
                        if let Some((style, on)) = v.committed {
                            if let Some(h) = v.host.as_mut() {
                                h.prompt.style = style;
                            }
                            if let Some(s) = v.import.as_mut() {
                                s.enabled = on;
                            }
                        }
                    }
                }
                if std::mem::take(&mut v.save_again) {
                    v.save(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The question about the user's own startup files, when they have some.
    fn import_section(&self, ui: Ui, cx: &Context<'_, Self>) -> Option<impl IntoElement> {
        let s = self.import.as_ref()?;
        s.source.as_ref()?;
        let found: Vec<&str> = s.files.iter().filter(|f| f.exists).map(|f| f.path.as_str()).collect();
        let main = *found.iter().find(|f| f.ends_with("rc")).or(found.last())?;
        let desc = match (&s.harvest, &s.failure) {
            (Some(h), _) => {
                let mut parts = Vec::new();
                let path = h.added_path().len();
                if path > 0 {
                    parts.push(format!("{path} PATH {}", if path == 1 { "folder" } else { "folders" }));
                }
                let n = h.vars.len();
                if n > 0 {
                    let word = if n == 1 { "variable" } else { "variables" };
                    parts.push(match h.vars.iter().find(|v| v.name == "EDITOR") {
                        Some(e) => format!("{n} {word} (EDITOR={})", e.value),
                        None => format!("{n} {word}"),
                    });
                }
                if parts.is_empty() {
                    "It sets nothing up that goatty's shell doesn't have already".to_string()
                } else {
                    format!("Brings {} into goatty's shell", shell_setup::names(&parts.iter().map(String::as_str).collect::<Vec<_>>()))
                }
            }
            (None, Some(f)) => format!("Couldn't check it: {}", f.message),
            (None, None) if self.import_checking => "Checking what it sets up…".to_string(),
            (None, None) => "The PATH folders and variables it sets up".to_string(),
        };
        let on = s.enabled;
        let mut sec = settings_section(ui, "Your dotfiles").desc(format!("goatty found {}", found.join(", "))).child(
            settings_row(ui, format!("Use what {main} sets up")).desc(desc).control(toggle(
                "import",
                on,
                ui,
                self.act(cx, move |v, cx| v.set_import(!on, cx)),
            )),
        );
        sec = match &self.import_error {
            Some(e) if e.starts_with("Saved") => sec.footer(e.clone()),
            Some(e) => sec.footer(format!("Couldn't change it: {e}")),
            None => {
                let own = s.harvest.as_ref().map_or(0, |h| h.aliases.iter().filter(|a| a.own).count());
                sec.footer(if own > 0 {
                    format!("Your prompt and plugin frameworks in it stay out of goatty's shell. Your {own} aliases: copy them under Settings → Aliases.")
                } else {
                    "Your prompt and plugin frameworks in it stay out of goatty's shell. Change it later under Settings → Dotfiles.".to_string()
                })
            }
        };
        Some(sec)
    }

    fn install(&mut self, cx: &mut Context<'_, Self>) {
        let (Some(link), Some(status)) = (self.link.clone(), self.status.as_ref()) else { return };
        let progress = Progress::new(status.tools.iter().filter(|t| self.chosen.contains(&t.id)));
        self.progress = Some(progress.clone());
        cx.notify();
        cx.spawn(async move |this, cx| {
            let weak = this.clone();
            let report = move |p: &Progress, cx: &mut App| {
                let _ = weak.update(cx, |v, cx| {
                    v.progress = Some(p.clone());
                    cx.notify();
                });
            };
            shell_setup::install(link, progress, cx, report).await;
        })
        .detach();
    }

    /// Set the host's prompt style.
    fn set_prompt(&mut self, style: PromptStyle, cx: &mut Context<'_, Self>) {
        let Some(host) = self.host.as_mut() else { return };
        if host.prompt.style == style {
            return;
        }
        host.prompt.style = style;
        self.save(cx);
    }

    fn installing(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| !p.finished)
    }

    fn installed(&self) -> bool {
        self.progress.as_ref().is_some_and(|p| p.finished)
    }

    /// Callback acting on this view.
    fn act(
        &self,
        cx: &Context<'_, Self>,
        f: impl Fn(&mut Self, &mut Context<'_, Self>) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let this = cx.entity().downgrade();
        move |_, cx| {
            let _ = this.update(cx, |v, cx| f(v, cx));
        }
    }

    // ---- Rendering ------------------------------------------------------------

    fn steps(&self, ui: Ui) -> impl IntoElement {
        div().flex().items_center().gap(px(8.)).children(Step::ALL.into_iter().enumerate().map(|(i, s)| {
            let (current, past) = (s == self.step, s < self.step);
            let dot =
                div()
                    .size(px(22.))
                    .rounded_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(text::CAPTION))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when(current, |d| d.bg(ui.accent).text_color(ui.on_accent))
                    .when(past, |d| d.bg(ui.accent_soft))
                    .when(!current && !past, |d| d.bg(ui.field).text_color(ui.muted))
                    .map(|d| {
                        if past {
                            d.child(Icon::Check.svg().size(px(12.)).text_color(ui.accent_text))
                        } else {
                            d.child((i + 1).to_string())
                        }
                    });
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .when(i > 0, |d| {
                    d.child(div().w(px(28.)).h(px(2.)).rounded_full().bg(if current || past { ui.accent_soft } else { ui.divider }))
                })
                .child(dot)
                .child(
                    div()
                        .text_size(px(text::LABEL))
                        .font_weight(if current { FontWeight::SEMIBOLD } else { FontWeight::NORMAL })
                        .text_color(if current { ui.text } else { ui.muted })
                        .child(s.label()),
                )
        }))
    }

    fn title(ui: Ui, title: impl Into<SharedString>, subtitle: impl Into<SharedString>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .child(div().text_size(px(text::TITLE)).font_weight(FontWeight::SEMIBOLD).child(title.into()))
            .child(div().text_size(px(text::BODY)).text_color(ui.muted).child(subtitle.into()))
    }

    fn look(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        let app = cx.global::<AppSettings>();
        let theme = app.file.value.theme.clone();
        let font_size = app.file.value.font.size;
        let themes = app.themes.all();
        let cards = themes.into_iter().map(|t| {
            let selected = if theme.follow_system { t.id == theme.light || t.id == theme.dark } else { t.id == theme.name };
            self.theme_card(ui, t, selected, cx)
        });
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(Self::title(
                ui,
                "Welcome to goatty",
                "A fast terminal with a shell that's ready to go. Two quick choices, then you're set.",
            ))
            .child(
                settings_section(ui, "Theme")
                    .child(
                        settings_row(ui, "Follow the system")
                            .desc("A light theme by day, a dark one by night, as your desktop switches")
                            .control(toggle("follow-system", theme.follow_system, ui, |_, cx| {
                                AppSettings::update(cx, |s| s.theme.follow_system = !s.theme.follow_system)
                            })),
                    )
                    .child(div().flex().flex_wrap().gap(px(10.)).p(px(14.)).children(cards)),
            )
            .child(settings_section(ui, "Text").child(settings_row(ui, "Size").desc("Ctrl+= and Ctrl+- change it in any window").control(
                stepper(
                    "font-size",
                    format!("{font_size:.0} px"),
                    ui,
                    |_, cx| AppSettings::update(cx, |s| s.font.size = (s.font.size - 1.0).max(goatty_config::FontSettings::MIN_SIZE)),
                    |_, cx| AppSettings::update(cx, |s| s.font.size = (s.font.size + 1.0).min(goatty_config::FontSettings::MAX_SIZE)),
                ),
            )))
    }

    fn theme_card(&self, ui: Ui, t: ThemeEntry, selected: bool, cx: &Context<'_, Self>) -> impl IntoElement {
        let col = t.colors;
        let c = |x: goatty_config::Color| -> gpui::Hsla { rgb(x.0).into() };
        let (id, dark) = (t.id.clone(), col.is_dark());
        div()
            .id(SharedString::from(format!("theme-{}", t.id)))
            .w(px(128.))
            .flex()
            .flex_col()
            .rounded(px(12.))
            .overflow_hidden()
            .border_2()
            .border_color(if selected { ui.accent } else { ui.divider })
            .cursor_pointer()
            .hover(|s| s.border_color(if selected { ui.accent } else { ui.border }))
            .child(
                div()
                    .h(px(46.))
                    .bg(c(col.background))
                    .px(px(10.))
                    .py(px(8.))
                    .flex()
                    .flex_col()
                    .justify_between()
                    .font_family(crate_mono(cx))
                    .text_size(px(11.))
                    .child(
                        div()
                            .flex()
                            .child(div().text_color(c(col.ansi[2])).child("❯ "))
                            .child(div().text_color(c(col.foreground)).child("ls")),
                    )
                    .child(div().flex().gap(px(3.)).children(col.ansi[1..7].iter().map(|x| div().size(px(9.)).rounded_full().bg(c(*x))))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(5.))
                    .h(px(28.))
                    .px(px(9.))
                    .bg(ui.card)
                    .text_size(px(text::CAPTION))
                    .text_color(if selected { ui.accent_text } else { ui.text })
                    .when(selected, |d| d.child(Icon::Check.svg().size(px(12.)).text_color(ui.accent_text)))
                    .child(div().truncate().child(SharedString::from(t.name.clone()))),
            )
            .on_click(move |_, _, cx| {
                let id = id.clone();
                AppSettings::update(cx, move |s| match (s.theme.follow_system, dark) {
                    (false, _) => s.theme.name = id.clone(),
                    (true, true) => s.theme.dark = id.clone(),
                    (true, false) => s.theme.light = id.clone(),
                });
            })
    }

    fn shell(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        let host = match &self.link {
            Some(l) if !l.is_local() => format!(" on {}", l.label()),
            _ => String::new(),
        };
        let mut page = div().flex().flex_col().gap(px(24.)).child(Self::title(
            ui,
            format!("Your shell{host}"),
            "goatty runs zsh with suggestions as you type, syntax colors, a clear prompt and searchable history. It doesn't change your dotfiles unless you ask it to.",
        ));
        if self.link.is_none() {
            return page.child(loading_card(ui, "Waiting for the terminal window to connect…"));
        }
        let Some(status) = &self.status else {
            return match &self.error {
                Some(e) => page.child(banner(ui, Tone::Danger, "Couldn't check the shell", vec![e.clone()])).child(div().flex().child(
                    button_with("retry", Some(Icon::Refresh), "Try again", ButtonKind::Normal, ui, self.act(cx, |v, cx| v.load(cx))),
                )),
                None => page.child(loading_card(ui, "Checking what's installed…")),
            };
        };
        if let Some(why) = &status.unmanaged {
            return page.child(banner(
                ui,
                Tone::Neutral,
                format!("{} starts as is", status.program),
                vec![
                    why.clone(),
                    "goatty's shell setup is for zsh. Install zsh to get it; everything else in goatty works either way.".into(),
                ],
            ));
        }
        let busy = self.installing() || self.installed();
        // The default tools first; optional ones (fzf) after them.
        let mut tools: Vec<_> = status.tools.iter().collect();
        tools.sort_by_key(|t| !t.default_install);
        let rows = tools.into_iter().map(|t| {
            let mut r = settings_row(ui, t.name.clone());
            r = match (&t.location, &t.unavailable) {
                (ToolLocation::System { .. }, _) => {
                    r.desc(format!("{} · already on your system", t.summary)).control(badge("Installed", Tone::Success, ui))
                }
                (ToolLocation::Managed { version }, _) if t.outdated().is_none() => {
                    r.desc(format!("{} · installed by goatty ({version})", t.summary)).control(badge("Installed", Tone::Success, ui))
                }
                (ToolLocation::Managed { version }, Some(why)) => r
                    .desc(format!("{} · goatty {version} (pinned {}: {why})", t.summary, t.version))
                    .control(badge("Outdated", Tone::Warning, ui)),
                (ToolLocation::Managed { version }, None) => {
                    let on = self.chosen.contains(&t.id);
                    let id = t.id.clone();
                    r.desc(format!("{} · update {version} → {} · {}", t.summary, t.version, setup::size(t.download)))
                        .disabled(busy)
                        .control(toggle(
                            SharedString::from(format!("tool-{}", t.id)),
                            on,
                            ui,
                            self.act(cx, move |v, cx| {
                                if !v.chosen.remove(&id) {
                                    v.chosen.insert(id.clone());
                                }
                                cx.notify();
                            }),
                        ))
                }
                (ToolLocation::Missing, Some(why)) => r.desc(why.clone()).control(badge("Not available", Tone::Neutral, ui)),
                (ToolLocation::Missing, None) => {
                    let on = self.chosen.contains(&t.id);
                    let id = t.id.clone();
                    let extra = if t.default_install { "" } else { " · optional" };
                    r.desc(format!("{} · {}{extra}", t.summary, setup::size(t.download))).disabled(busy).control(toggle(
                        SharedString::from(format!("tool-{}", t.id)),
                        on,
                        ui,
                        self.act(cx, move |v, cx| {
                            if !v.chosen.remove(&id) {
                                v.chosen.insert(id.clone());
                            }
                            cx.notify();
                        }),
                    ))
                }
            };
            r
        });
        page = page.child(
            settings_section(ui, "Tools")
                .desc("Each one is optional: goatty works without them")
                .children(rows)
                .footer("Downloads go to ~/.local/share/goatty and are checked against the checksums goatty pins. No sudo."),
        );
        if let Some(p) = &self.progress {
            page = page.child(settings_window::progress_card(ui, p));
        }
        if let Some(sec) = self.import_section(ui, cx) {
            page = page.child(sec);
        }
        if let Some(sec) = self.everywhere_section(ui, cx) {
            page = page.child(sec);
        }
        if let Some(h) = &self.host {
            let style = h.prompt.style;
            let colors = cx.global::<AppSettings>().colors;
            let font = crate::fonts::terminal(&crate_mono(cx));
            let card = |s: PromptStyle| {
                let mut sample = goatty_config::PromptSettings {
                    style: s,
                    two_line: false,
                    show: goatty_config::prompt::ModuleSet::Essential,
                    modules: Default::default(),
                    duration_threshold_ms: 0,
                    ..h.prompt.clone()
                };
                sample.directory.truncation_length = 1;
                let preview = crate::ui::prompt_preview::Preview { p: &sample, theme: &colors, starship: true, line_height: 20. };
                let line = div()
                    .h(px(64.))
                    .px(px(12.))
                    .flex()
                    .flex_col()
                    .justify_center()
                    .overflow_hidden()
                    .bg(gpui::Hsla::from(rgb(colors.background.0)))
                    .font(font.clone())
                    .text_size(px(12.))
                    .children(preview.prompt(crate::ui::prompt_preview::Moment::Fresh, div()));
                choice(ui, SharedString::from(format!("prompt-{}", s.label())), s.label(), style == s, line)
                    .min_w(px(150.))
                    .on_click(cx.listener(move |v: &mut Self, _: &gpui::ClickEvent, _, cx| v.set_prompt(s, cx)))
            };
            let mut sec = settings_section(ui, "Prompt").desc("Pick a look: there's more under Settings → Prompt");
            if let Some(e) = &self.prompt_error {
                sec = sec.footer(if e.starts_with("Saved") { e.clone() } else { format!("Couldn't change it: {e}") });
            }
            page = page.child(sec.child(div().flex().flex_wrap().gap(px(12.)).p(px(14.)).children(PromptStyle::ALL.map(card))));
        }
        page
    }

    fn ready(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        let overrides = cx.global::<AppSettings>().client.keybindings.clone();
        let key = |action: &str| goatty_config::keys::keys_for(action, &overrides).into_iter().next();
        let below = match key("split_down") {
            Some(k) => format!("Side by side; {} splits below", crate::ui::controls::pretty_keys(&k)),
            None => "Side by side, or below".into(),
        };
        let shortcut = |label: &'static str, desc: &str, action: &str| {
            settings_row(ui, label).desc(desc.to_owned()).control(match key(action) {
                Some(k) => keycaps(&k, ui).into_any_element(),
                None => badge("Not bound", Tone::Neutral, ui).into_any_element(),
            })
        };
        let installed: Vec<&str> =
            self.progress.iter().flat_map(|p| p.tools.iter().filter(|t| t.2 == shell_setup::Step::Done).map(|t| t.1.as_str())).collect();
        let subtitle = if installed.is_empty() {
            "That's all. These are the shortcuts worth knowing; the rest is in the command palette.".to_string()
        } else {
            format!(
                "{} {} ready in your shells. These are the shortcuts worth knowing.",
                shell_setup::names(&installed),
                if installed.len() == 1 { "is" } else { "are" }
            )
        };
        div()
            .flex()
            .flex_col()
            .gap(px(24.))
            .child(Self::title(ui, "You're set", subtitle))
            .child(
                settings_section(ui, "Shortcuts")
                    .child(shortcut("Command palette", "Every command, setting and host, by name", "command_palette"))
                    .child(shortcut("New tab", "In the current tab's directory", "new_tab"))
                    .child(shortcut("Split the pane", &below, "split_right"))
                    .child(shortcut("Connect to a host", "Your SSH hosts, with goatty's shell there too", "connect_to_host"))
                    .child(shortcut("Settings", "Everything else, with a search", "open_settings")),
            )
            .child(
                settings_section(ui, "More")
                    .child(settings_row(ui, "Settings").icon(Icon::Sliders).desc("Fonts, tabs, prompt, history, plugins").on_click(
                        "more-settings",
                        |_, window, cx| {
                            settings_window::open(cx);
                            window.remove_window();
                        },
                    ))
                    .child(
                        settings_row(ui, "Diagnostics").icon(Icon::Activity).desc("What the shell loads, and how fast it starts").on_click(
                            "more-diagnostics",
                            |_, window, cx| {
                                settings_window::open_at(cx, Some(Page::Diagnostics));
                                window.remove_window();
                            },
                        ),
                    ),
            )
    }

    fn footer(&self, ui: Ui, cx: &Context<'_, Self>) -> impl IntoElement {
        let back = match self.step {
            Step::Look => None,
            Step::Shell => Some(Step::Look),
            Step::Ready => Some(Step::Shell),
        };
        let primary = match self.step {
            Step::Look => button("next", "Continue", ButtonKind::Primary, ui, self.act(cx, |v, cx| v.go(Step::Shell, cx))),
            Step::Shell if self.installing() => {
                let line = self.progress.as_ref().map(Progress::status_line).unwrap_or_default();
                button_with("next", Some(Icon::Loader), line, ButtonKind::Primary, ui, |_, _| {}).opacity(0.7)
            }
            Step::Shell if !self.chosen.is_empty() && !self.installed() && self.status.as_ref().is_some_and(|s| s.managed) => {
                let s = self.status.as_ref();
                let bytes: u64 = s.map(|s| s.tools.iter().filter(|t| self.chosen.contains(&t.id)).map(|t| t.download).sum()).unwrap_or(0);
                let n = self.chosen.len();
                let verb = match (
                    s.is_some_and(|s| s.tools.iter().any(|t| self.chosen.contains(&t.id) && t.outdated().is_none())),
                    s.is_some_and(|s| s.tools.iter().any(|t| self.chosen.contains(&t.id) && t.outdated().is_some())),
                ) {
                    (true, true) => "Install & update",
                    (false, true) => "Update",
                    _ => "Install",
                };
                let label = format!("{verb} {n} {} · {}", if n == 1 { "tool" } else { "tools" }, setup::size(bytes));
                button_with("next", Some(Icon::Download), label, ButtonKind::Primary, ui, self.act(cx, |v, cx| v.install(cx)))
            }
            Step::Shell => button("next", "Continue", ButtonKind::Primary, ui, self.act(cx, |v, cx| v.go(Step::Ready, cx))),
            Step::Ready => button("next", "Start using goatty", ButtonKind::Primary, ui, |window, _| window.remove_window()),
        };
        // "Not now" on the shell step: go on without installing.
        let skip_install = self.step == Step::Shell && !self.chosen.is_empty() && !self.installing() && !self.installed();
        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(28.))
            .py(px(16.))
            .border_t_1()
            .border_color(ui.divider)
            .bg(ui.sidebar)
            .when(self.step != Step::Ready, |d| {
                d.child(button("skip", "Skip setup", ButtonKind::Ghost, ui, |window, _| window.remove_window()))
            })
            .child(div().flex_1())
            .when_some(back, |d, step| d.child(button("back", "Back", ButtonKind::Normal, ui, self.act(cx, move |v, cx| v.go(step, cx)))))
            .when(skip_install, |d| {
                d.child(button("not-now", "Not now", ButtonKind::Normal, ui, self.act(cx, |v, cx| v.go(Step::Ready, cx))))
            })
            .child(primary)
    }
}

impl Render for Welcome {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let ui = Ui::get(cx);
        let cx: &Context<'_, Self> = cx;
        let body = match self.step {
            Step::Look => self.look(ui, cx).into_any_element(),
            Step::Shell => self.shell(ui, cx).into_any_element(),
            Step::Ready => self.ready(ui, cx).into_any_element(),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(ui.bg)
            .text_color(ui.text)
            .font_family(cx.global::<AppSettings>().ui_font.clone())
            .key_context(CONTEXT)
            .track_focus(&self.focus)
            .on_action(cx.listener(Self::close))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(28.))
                    .pt(px(20.))
                    .pb(px(12.))
                    .child(
                        div()
                            .size(px(30.))
                            .rounded(px(9.))
                            .bg(ui.accent)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::Terminal.svg().size(px(17.)).text_color(ui.on_accent)),
                    )
                    .child(div().flex_1())
                    .child(self.steps(ui)),
            )
            .child(
                div().id("welcome-page").flex_1().overflow_y_scroll().child(
                    div().flex().justify_center().px(px(28.)).child(div().w_full().max_w(px(700.)).pt(px(12.)).pb(px(32.)).child(body)),
                ),
            )
            .child(self.footer(ui, cx))
    }
}

fn crate_mono(cx: &App) -> SharedString {
    cx.global::<AppSettings>().client.font.family.clone().unwrap_or_else(|| "monospace".into()).into()
}

fn loading_card(ui: Ui, t: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap(px(12.))
        .p(px(18.))
        .rounded(px(radius::CARD))
        .bg(ui.card)
        .border_1()
        .border_color(ui.divider)
        .child(Icon::Loader.svg().size(px(18.)).text_color(ui.accent_text))
        .child(div().text_size(px(text::BODY)).text_color(ui.muted).child(t))
}

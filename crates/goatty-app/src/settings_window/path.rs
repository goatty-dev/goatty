//! The PATH page (#90): the folders goatty's zsh adds to PATH on the
//! connected host (`shell.path`, edited in the host draft like every host
//! setting), with what became of each one, and the whole PATH a new shell
//! gets, each folder with where it came from. The host starts a managed
//! shell to find out (`goatty_shell::path_report`), like Diagnostics does.

use goatty_config::{HostSettings, normalize_path_entry, path_entry_problem};
use goatty_shell::path_report::{self, Entry, FolderState, Origin, Report};
use gpui::{AnyElement, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Subscription, Window, div, prelude::*, px};

use super::kit::{self, card, chip, divider, row, search_field, section};
use super::{Page, SettingsWindow};
use crate::toast::{self, Toast};
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button, button_with, icon_button};
use crate::ui::focus;
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

const KEY: &str = "shell.path";

pub struct PathPage {
    pub report: Option<Report>,
    pub loading: bool,
    pub error: Option<String>,
    /// The host generation the report was made with.
    generation: Option<u32>,
    /// "Find a folder" over the whole PATH.
    filter: Entity<TextInput>,
    /// Only folders that don't exist.
    missing_only: bool,
    _subs: Vec<Subscription>,
}

impl PathPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Find a folder on PATH", "", cx).bare());
        let subs = vec![cx.subscribe(&filter, |_, _, ev: &TextInputEvent, cx| {
            if let TextInputEvent::Changed(_) = ev {
                cx.notify();
            }
        })];
        Self { report: None, loading: false, error: None, generation: None, filter, missing_only: false, _subs: subs }
    }

    /// Forget the report (another host).
    pub fn reset(&mut self) {
        self.report = None;
        self.error = None;
        self.generation = None;
    }
}

impl SettingsWindow {
    /// Ask the host for its PATH again (starts a shell there).
    pub fn load_path_report(&mut self, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else {
            self.path.error = Some("This window isn't connected to a goatty host.".into());
            return;
        };
        if self.path.loading {
            return;
        }
        self.path.loading = true;
        let generation = self.host.as_ref().and_then(|h| h.generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let label = link.label();
            let result = cx.background_executor().spawn(async move { link.path_report() }).await;
            let _ = this.update(cx, |view, cx| {
                view.path.loading = false;
                match result {
                    Ok(r) => {
                        view.path.report = Some(r);
                        view.path.error = None;
                        view.path.generation = generation;
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        view.path.error = Some(if e.contains("unknown variant") {
                            format!("goattyd on {label} is older than this goatty and can't report its PATH. Update it to see where each folder comes from.")
                        } else {
                            e
                        });
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The host's settings changed (applied, rolled back): the report is old.
    pub(super) fn path_host_changed(&mut self, cx: &mut Context<'_, Self>) {
        let generation = self.host.as_ref().and_then(|h| h.generation);
        if self.path.report.is_some() && self.path.generation != generation {
            if self.page == Page::Path {
                self.load_path_report(cx);
            } else {
                self.path.reset();
            }
        }
    }

    pub(super) fn path_page(&self, ui: Ui, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        let (notes, h) = self.host_status(ui);
        let mut page = div().flex().flex_col().gap(px(28.)).children(notes);
        let Some(h) = h else { return page.into_any_element() };
        if !crate::shell_path::managed_zsh(&h) {
            page = page.child(div().flex().flex_col().gap(px(10.)).child(banner(
                ui,
                Tone::Warning,
                "Your folders aren't added",
                vec!["Only goatty's managed zsh adds them, and it isn't used on this host. Turn it on in Shell.".into()],
            )));
        }
        page = page.child(self.path_folders(ui, &h, cx));
        page = page.child(self.path_current(ui, &h, window, cx));
        page.into_any_element()
    }

    /// The editable list (`shell.path`) and the "Add a folder" row.
    fn path_folders(&self, ui: Ui, h: &HostSettings, cx: &Context<'_, Self>) -> impl IntoElement {
        let mono = kit::mono(cx);
        let applied = self.host.as_ref().map(|v| &v.loaded.value.shell.path);
        let report = self.path.report.as_ref();
        let n = h.shell.path.len();
        let mut s = section(ui, "Your folders").desc("goatty adds these to the end of PATH in new shells, in this order");
        if n == 0 {
            s = s.child(kit::empty(
                ui,
                Icon::FolderOpen,
                "No folders added",
                "Add a folder below and the programs in it work in every new shell, without editing your dotfiles.",
            ));
        }
        for (i, entry) in h.shell.path.iter().enumerate() {
            let new = applied.is_some_and(|a| !a.contains(entry));
            let folder = report.and_then(|r| r.folders.iter().find(|f| f.entry == *entry));
            let (status, note) = match (new, folder.map(|f| &f.state)) {
                (true, _) => (Some(badge("Not applied yet", Tone::Accent, ui)), "Apply to add it to new shells".to_string()),
                (false, Some(FolderState::Used)) => {
                    let at = folder
                        .and_then(|f| report.and_then(|r| r.entries.iter().position(|e| same_dir(&e.dir, &f.dir))))
                        .map_or(String::new(), |p| format!(", folder {} of {}", p + 1, report.map_or(0, |r| r.entries.len())));
                    (None, format!("On PATH{at}"))
                }
                (false, Some(FolderState::Missing)) => {
                    (Some(badge("Doesn't exist", Tone::Warning, ui)), "Shells skip it until the folder exists".into())
                }
                (false, Some(FolderState::NotAFolder)) => {
                    (Some(badge("Not a folder", Tone::Warning, ui)), "It's a file: shells skip it".into())
                }
                (false, Some(FolderState::Earlier { origin })) => (
                    Some(badge("Already on PATH", Tone::Neutral, ui)),
                    format!("{} adds it earlier, so this entry changes nothing", origin_text(origin)),
                ),
                (false, Some(FolderState::Dropped)) => {
                    (Some(badge("Not on PATH", Tone::Warning, ui)), "Something that runs after goatty removed it again".into())
                }
                (false, Some(FolderState::Unchecked) | None) => (None, String::new()),
            };
            let up = (i > 0).then(|| {
                icon_button(
                    ("path-up", i),
                    Icon::ChevronUp,
                    format!("Move {entry} up"),
                    ui,
                    self.act(cx, move |v, cx| {
                        v.edit_host(cx, |h| h.shell.path.swap(i - 1, i));
                    }),
                )
            });
            let down = (i + 1 < n).then(|| {
                icon_button(
                    ("path-down", i),
                    Icon::ChevronDown,
                    format!("Move {entry} down"),
                    ui,
                    self.act(cx, move |v, cx| {
                        v.edit_host(cx, |h| h.shell.path.swap(i, i + 1));
                    }),
                )
            });
            let remove = entry.clone();
            let trash = icon_button(
                ("path-remove", i),
                Icon::Trash,
                format!("Remove {entry} from PATH"),
                ui,
                self.act_window(cx, move |v, window, cx| {
                    // The row is going: focus waits on the Add button.
                    focus::hand_over(&v.focus, focus::Handover::Control("path-add".into()), window, cx);
                    let remove = remove.clone();
                    v.edit_host(cx, move |h| h.shell.path.retain(|d| *d != remove));
                }),
            );
            s = s.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .min_h(px(56.))
                    .px(px(16.))
                    .py(px(8.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .flex_1()
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_size(px(text::BODY))
                                            .font_family(mono.clone())
                                            .text_color(ui.text)
                                            .truncate()
                                            .child(SharedString::from(entry.clone())),
                                    )
                                    .children(status),
                            )
                            .when(!note.is_empty(), |d| {
                                d.child(div().text_size(px(text::SMALL)).text_color(ui.muted).truncate().child(SharedString::from(note)))
                            }),
                    )
                    .child(div().flex().items_center().gap(px(2.)).child(slot(up)).child(slot(down)).child(trash)),
            );
        }
        let entry = self.path_entry.clone();
        let add = self.act(cx, move |v, cx| {
            let text = entry.read(cx).text().to_string();
            v.add_path_entry(&text, cx);
        });
        let mut controls = div().flex().items_center().gap(px(8.));
        if self.link.as_ref().is_some_and(super::HostLink::is_local) {
            controls = controls.child(button_with(
                "path-browse",
                Some(Icon::FolderOpen),
                "Browse…",
                ButtonKind::Ghost,
                ui,
                self.act(cx, |v, cx| v.browse_path_entry(cx)),
            ));
        }
        controls = controls.child(button("path-add", "Add", ButtonKind::Normal, ui, add));
        s.child(row(ui, "Add a folder").desc(self.path_entry_hint(h, cx)).control(controls).below(self.path_entry.clone()))
            .footer("Also in the command palette: \"Add a folder to PATH\". Running shells pick changes up when they reload.")
    }

    /// What adding the typed folder would do, as the user types.
    fn path_entry_hint(&self, h: &HostSettings, cx: &Context<'_, Self>) -> String {
        let text = self.path_entry.read(cx).text().to_string();
        if text.trim().is_empty() {
            return "A full path: /… or ~/… · Enter adds it".into();
        }
        if let Some(problem) = path_entry_problem(&text) {
            return problem.into();
        }
        let dir = normalize_path_entry(&text);
        if h.shell.path.contains(&dir) {
            return "Already in your list".into();
        }
        let report = self.path.report.as_ref();
        let home = report.and_then(|r| r.home.as_deref()).map(std::path::Path::new);
        let expanded = path_report::expand(&dir, home);
        if let Some(e) = report.and_then(|r| r.entries.iter().find(|e| same_dir(&e.dir, &expanded))) {
            return format!("Already on PATH: {} adds it", origin_text(&e.origin));
        }
        if self.link.as_ref().is_some_and(super::HostLink::is_local) {
            let local = path_report::expand(&dir, std::env::var_os("HOME").as_deref().map(std::path::Path::new));
            let p = std::path::Path::new(&local);
            if !p.exists() {
                return "This folder doesn't exist yet: shells skip it until it does".into();
            }
            if !p.is_dir() {
                return "That's a file, not a folder".into();
            }
        }
        "Enter adds it".into()
    }

    /// Pick a folder on this computer for the "Add a folder" field.
    fn browse_path_entry(&mut self, cx: &mut Context<'_, Self>) {
        let rx = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Add to PATH".into()),
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else { return };
            let Some(p) = paths.into_iter().next() else { return };
            let _ = this.update(cx, |view, cx| {
                let text = goatty_shell::setup::tilde(&p);
                view.path_entry.update(cx, |input, cx| input.set_text(text, cx));
                cx.notify();
            });
        })
        .detach();
    }

    /// The PATH a new shell gets, each folder with where it came from.
    fn path_current(&self, ui: Ui, h: &HostSettings, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        let refresh = button_with(
            "path-refresh",
            Some(Icon::Refresh),
            if self.path.loading { "Checking…" } else { "Check again" },
            ButtonKind::Ghost,
            ui,
            self.act(cx, |v, cx| v.load_path_report(cx)),
        );
        let s = section(ui, "PATH in a new shell")
            .desc(format!("Where shells on {} look for programs: the first folder that has it wins", self.host_label()))
            .action(refresh)
            .plain();
        let Some(r) = &self.path.report else {
            return match &self.path.error {
                Some(e) => s.child(banner(ui, Tone::Danger, "Couldn't read PATH", vec![e.clone()])).into_any_element(),
                None => s
                    .child(card(ui).child(kit::empty(
                        ui,
                        Icon::Loader,
                        "Starting a shell to read its PATH…",
                        "goatty starts your shell the way a new tab does and asks where each folder came from.",
                    )))
                    .into_any_element(),
            };
        };
        let mut s = s;
        if let Some(e) = &self.path.error {
            s = s.child(banner(ui, Tone::Danger, "Couldn't read PATH again", vec![e.clone()]));
        }
        if let Some(p) = &r.problem {
            s = s.child(banner(ui, Tone::Neutral, "Where these come from is unknown", vec![p.clone()]));
        }
        let query = self.path.filter.read(cx).text().trim().to_string();
        let home = r.home.as_deref().map(std::path::Path::new);
        let wanted = path_report::expand(&normalize_path_entry(&query), home);
        let missing = r.entries.iter().filter(|e| !e.exists).count();
        let missing_only = self.path.missing_only;
        let shown: Vec<(usize, &Entry)> = r
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| !missing_only || !e.exists)
            .filter(|(_, e)| query.is_empty() || e.dir.contains(&query) || e.shown.contains(&query) || same_dir(&e.dir, &wanted))
            .collect();
        let toggle_missing = self.act(cx, |v, cx| {
            v.path.missing_only = !v.path.missing_only;
            cx.notify();
        });
        let show_all = self.act(cx, |v, cx| {
            v.path.missing_only = false;
            cx.notify();
        });
        s = s.child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().flex_1().min_w_0().child(search_field(ui, "path-search", &self.path.filter, window, cx)))
                .child(chip(ui, "path-all", "All", Some(r.entries.len()), !missing_only, show_all))
                .child(chip(ui, "path-missing", "Doesn't exist", Some(missing), missing_only, toggle_missing)),
        );
        // A folder typed into the search that isn't on PATH: say so, offer to add it.
        let looks_like_dir = query.starts_with('/') || query == "~" || query.starts_with("~/");
        if looks_like_dir && !r.entries.iter().any(|e| same_dir(&e.dir, &wanted)) {
            let in_list = h.shell.path.contains(&normalize_path_entry(&query));
            let mut note = row(ui, format!("{query} isn't on PATH")).icon(Icon::Info);
            if in_list {
                note = note.desc("It's in your folders, but not applied yet or skipped");
            } else if path_entry_problem(&query).is_none() {
                let q = query.clone();
                note = note.desc("Add it to your folders to use the programs in it").control(button(
                    "path-search-add",
                    "Add it",
                    ButtonKind::Normal,
                    ui,
                    self.act(cx, move |v, cx| v.add_path_entry(&q, cx)),
                ));
            }
            s = s.child(card(ui).child(note));
        }
        if shown.is_empty() {
            let (title, desc) = if missing_only && query.is_empty() {
                ("Every folder exists", "Nothing on PATH points to a folder that's gone.")
            } else {
                ("No folder matches", "Try part of a path, like bin or .cargo.")
            };
            return s.child(card(ui).child(kit::empty(ui, Icon::Search, title, desc))).into_any_element();
        }
        let mono = kit::mono(cx);
        let mut list = card(ui);
        for (k, (i, e)) in shown.iter().enumerate() {
            if k > 0 {
                list = list.child(divider(ui));
            }
            let origin = match &e.origin {
                Origin::Settings => badge("Your folders", Tone::Accent, ui),
                o => badge(origin_label(o), Tone::Neutral, ui),
            };
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .min_h(px(44.))
                    .px(px(16.))
                    .py(px(6.))
                    .child(
                        div()
                            .w(px(28.))
                            .flex_shrink_0()
                            .text_size(px(text::SMALL))
                            .text_color(ui.faint)
                            .child(SharedString::from(format!("{}", i + 1))),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(px(text::LABEL))
                            .font_family(mono.clone())
                            .text_color(if e.exists { ui.text } else { ui.muted })
                            .truncate()
                            .child(SharedString::from(e.shown.clone())),
                    )
                    .when(!e.exists, |d| d.child(badge("Doesn't exist", Tone::Warning, ui)))
                    .child(origin),
            );
        }
        s = s.child(list);
        if missing > 0 && !missing_only {
            s = s.child(kit::hint(ui, "Folders that don't exist are harmless, but usually left over from a tool you removed: the file that adds them is shown on the right.").px(px(4.)));
        }
        s.into_any_element()
    }
}

/// A button, or the room it takes (the first row can't move up), so the
/// buttons line up from row to row.
fn slot(button: Option<impl IntoElement>) -> AnyElement {
    match button {
        Some(b) => b.into_any_element(),
        None => div().size(px(30.)).flex_shrink_0().into_any_element(),
    }
}

/// The same folder, give or take a trailing `/`.
fn same_dir(a: &str, b: &str) -> bool {
    std::path::Path::new(a) == std::path::Path::new(b)
}

/// Short, for the badge: `~/.zprofile`, `inherited`.
fn origin_label(o: &Origin) -> String {
    match o {
        Origin::Inherited => "Inherited".into(),
        Origin::Tools => "goatty's tools".into(),
        Origin::Plugins => "A plugin".into(),
        Origin::Later => "After startup".into(),
        o => o.label(),
    }
}

/// In a sentence: "Your ~/.zprofile", "The system's /etc/zsh/zprofile".
fn origin_text(o: &Origin) -> String {
    match o {
        Origin::Inherited => "The environment goatty started in".into(),
        Origin::System { file } => format!("The system's {file}"),
        Origin::User { file } => format!("Your {file}"),
        Origin::Settings => "goatty's settings".into(),
        Origin::Tools => "goatty's tools folder".into(),
        Origin::Plugins => "A plugin".into(),
        Origin::Later => "Something after startup".into(),
    }
}

impl SettingsWindow {
    /// Add `text` to shell.path, or say why it can't be.
    pub(super) fn add_path_entry(&mut self, text: &str, cx: &mut Context<'_, Self>) {
        if let Some(problem) = path_entry_problem(text) {
            toast::show(cx, Toast::warning(format!("Can't add {:?} to PATH", text.trim())).body(problem).key(KEY));
            return;
        }
        let dir = normalize_path_entry(text);
        if self.host_value().is_some_and(|h| h.shell.path.contains(&dir)) {
            toast::show(cx, Toast::info(format!("{dir} is in your folders already")).key(KEY));
            return;
        }
        self.edit_host(cx, |h| h.shell.path.push(dir));
        self.path_entry.update(cx, |input, cx| input.set_text("", cx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folders_match_with_or_without_a_trailing_slash() {
        assert!(same_dir("/home/u/.cargo/bin/", "/home/u/.cargo/bin"));
        assert!(!same_dir("/home/u/bin", "/home/u/bin2"));
        assert_eq!(origin_text(&Origin::User { file: "~/.zprofile".into() }), "Your ~/.zprofile");
    }
}

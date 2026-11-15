//! The Dotfiles page: what goatty's zsh uses from the user's own startup
//! files on the connected host. The import (`shell.import`, on by
//! default): goatty runs their login shell in a separate process and
//! brings the PATH folders and variables it adds into new shells, each one
//! switchable (`goatty_shell::import`). Their aliases are copied once, on
//! the Aliases page (this page links there). Below that, the advanced part:
//! running `.zshenv`/`.zprofile` and (guarded) `.zshrc` in goatty's shell
//! itself.

use goatty_config::{HostSettings, ImportFrom};
use goatty_shell::import::{Harvest, Status};
use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Subscription, Window, div, prelude::*, px,
};

use super::kit::{self, card, chip, divider, opt, row, search_field, section};
use super::{Page, SettingsWindow};
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button_with, toggle};
use crate::ui::focus;
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, text};
use crate::ui::text_input::{TextInput, TextInputEvent};

/// Which entries the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Show {
    #[default]
    All,
    Off,
}

pub struct DotfilesPage {
    pub status: Option<Status>,
    pub loading: bool,
    /// The host runs the user's shell right now.
    pub checking: bool,
    pub error: Option<String>,
    /// Asked for a check once for this host (no harvest yet).
    auto_checked: bool,
    /// Numbers requests: a reply to an older one (another host, or before
    /// a reset) is dropped.
    request: u64,
    /// The host's settings changed while a request ran: ask again after it.
    stale: bool,
    /// The host generation the status belongs to.
    generation: Option<u32>,
    filter: Entity<TextInput>,
    show: Show,
    _subs: Vec<Subscription>,
}

impl DotfilesPage {
    pub fn new(cx: &mut Context<'_, SettingsWindow>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Find a folder or variable", "", cx).bare());
        let subs = vec![cx.subscribe(&filter, |_, _, ev: &TextInputEvent, cx| {
            if let TextInputEvent::Changed(_) = ev {
                cx.notify();
            }
        })];
        Self {
            status: None,
            loading: false,
            checking: false,
            error: None,
            auto_checked: false,
            request: 0,
            stale: false,
            generation: None,
            filter,
            show: Show::default(),
            _subs: subs,
        }
    }

    /// Forget what the host said (another host). A request still running
    /// is dropped when it replies.
    pub fn reset(&mut self) {
        self.status = None;
        self.error = None;
        self.auto_checked = false;
        self.generation = None;
        self.loading = false;
        self.checking = false;
        self.stale = false;
        self.request += 1;
    }
}

/// One entry of the list: PATH or a variable.
struct Item {
    /// Unique among items (the element ids).
    key: String,
    /// The variable (`PATH` too): switching it adds or removes it in
    /// `shell.import.skip_vars`.
    name: String,
    /// Lines of the value (PATH: one folder each).
    value: Vec<String>,
    kind: &'static str,
    on: bool,
}

fn items(h: &Harvest, s: &HostSettings) -> Vec<Item> {
    let imp = &s.shell.import;
    let mut out = Vec::new();
    let added = h.added_path();
    if !added.is_empty() {
        out.push(Item {
            key: "path".into(),
            name: "PATH".into(),
            value: added.iter().map(|d| goatty_shell::setup::tilde(std::path::Path::new(d))).collect(),
            kind: "PATH",
            on: !imp.skip_vars.iter().any(|n| n == "PATH"),
        });
    }
    out.extend(h.vars.iter().map(|v| Item {
        key: format!("var-{}", v.name),
        name: v.name.clone(),
        value: vec![v.value.clone()],
        kind: "Variable",
        on: !imp.skip_vars.contains(&v.name),
    }));
    out
}

/// `3 min ago`.
fn ago(secs: u64) -> String {
    match secs {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", secs / 60),
        3600..86400 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

impl SettingsWindow {
    /// Ask the host what the import has; `refresh`: run the user's shell first.
    pub fn load_dotfiles(&mut self, refresh: bool, cx: &mut Context<'_, Self>) {
        let Some(link) = self.link.clone() else {
            self.dotfiles.error = Some("This window isn't connected to a goatty host.".into());
            return;
        };
        if self.dotfiles.loading || self.dotfiles.checking {
            return;
        }
        if refresh {
            self.dotfiles.checking = true;
        } else {
            self.dotfiles.loading = true;
        }
        self.dotfiles.request += 1;
        let request = self.dotfiles.request;
        let generation = self.host.as_ref().and_then(|h| h.generation);
        cx.notify();
        cx.spawn(async move |this, cx| {
            let label = link.label();
            let result = cx.background_executor().spawn(async move { link.import_status(refresh) }).await;
            let _ = this.update(cx, |view, cx| {
                let d = &mut view.dotfiles;
                if d.request != request {
                    return;
                }
                d.loading = false;
                d.checking = false;
                if std::mem::take(&mut d.stale) {
                    view.load_dotfiles(false, cx);
                    return;
                }
                match result {
                    Ok(s) => {
                        // Nothing checked yet, but there are files: look once, so the list isn't empty.
                        let check = !refresh
                            && !d.auto_checked
                            && s.harvest.is_none()
                            && s.failure.is_none()
                            && s.source.is_some()
                            && s.files.iter().any(|f| f.exists);
                        d.status = Some(s);
                        d.error = None;
                        d.generation = generation;
                        if check {
                            d.auto_checked = true;
                            view.load_dotfiles(true, cx);
                        }
                    }
                    Err(e) => {
                        let e = format!("{e:#}");
                        d.error = Some(if e.contains("unknown variant") {
                            format!("goattyd on {label} is older than this goatty and can't import your shell setup. Update it to use this page.")
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

    /// The host's settings changed (applied, rolled back): ask again.
    pub(super) fn dotfiles_host_changed(&mut self, cx: &mut Context<'_, Self>) {
        let generation = self.host.as_ref().and_then(|h| h.generation);
        if self.dotfiles.loading || self.dotfiles.checking {
            self.dotfiles.stale = true;
        } else if self.dotfiles.status.is_some() && self.dotfiles.generation != generation {
            if self.page == super::Page::Dotfiles {
                self.load_dotfiles(false, cx);
            } else {
                self.dotfiles.reset();
            }
        }
    }

    pub(super) fn dotfiles_page(&self, ui: Ui, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        self.host_page(ui, |h| {
            let managed = crate::shell_path::managed_zsh(h);
            vec![
                self.import_section(ui, h, managed, cx),
                self.imported_list(ui, h, window, cx),
                self.aliases_offer(ui, h, cx),
                self.startup_files_section(ui, h, managed, cx),
            ]
        })
    }

    fn import_section(&self, ui: Ui, h: &HostSettings, managed: bool, cx: &Context<'_, Self>) -> AnyElement {
        let imp = &h.shell.import;
        let this = cx.entity().downgrade();
        let files = |kind: &str| match kind {
            "zsh" => "~/.zshenv, ~/.zprofile, ~/.zshrc",
            _ => "~/.profile, ~/.bash_profile, ~/.bashrc",
        };
        section(ui, "Import from your shell")
            .desc("Tools you installed and variables you set, without running your startup files in goatty's shell")
            .child(
                row(ui, "Use what your shell sets up")
                    .icon(Icon::Download)
                    .desc(if managed {
                        "New shells get the PATH folders and variables listed below"
                    } else {
                        "Only with goatty's managed zsh: turn it on in Shell"
                    })
                    .disabled(!managed)
                    .control(toggle("import-on", imp.enabled, ui, self.host(cx, |h| h.shell.import.enabled = !h.shell.import.enabled))),
            )
            .child(row(ui, "Read from").disabled(!managed).control(self.select(
                "import-from",
                "Read from",
                vec![
                    opt(ImportFrom::Auto, "Automatic").desc("Your login shell's files: zsh's, or bash's when zsh has none"),
                    opt(ImportFrom::Zsh, "zsh").desc(files("zsh")),
                    opt(ImportFrom::Bash, "bash").desc(files("bash")),
                ],
                &imp.from,
                cx,
                move |from, _, cx| {
                    let _ = this.update(cx, |v, cx| v.edit_host(cx, |h| h.shell.import.from = from));
                },
            )))
            .footer("goatty runs your shell without a terminal and notes the PATH folders and variables it adds; your prompt and plugin frameworks never run in goatty's shell. It checks again when your startup files change.")
            .into_any_element()
    }

    /// What the host's last check found, each entry switchable.
    fn imported_list(&self, ui: Ui, h: &HostSettings, window: &Window, cx: &Context<'_, Self>) -> AnyElement {
        let d = &self.dotfiles;
        let check = button_with(
            "import-check",
            Some(Icon::Refresh),
            if d.checking { "Checking…" } else { "Check again" },
            ButtonKind::Ghost,
            ui,
            self.act(cx, |v, cx| v.load_dotfiles(true, cx)),
        );
        let mut s = section(ui, "What your shell sets up").action(check).plain();
        let Some(st) = &d.status else {
            return match &d.error {
                Some(e) => s.child(banner(ui, Tone::Danger, "Couldn't ask the host", vec![e.clone()])).into_any_element(),
                None => s.child(card(ui).child(kit::empty(ui, Icon::Loader, "Asking the host…", ""))).into_any_element(),
            };
        };
        if let Some(e) = &d.error {
            s = s.child(banner(ui, Tone::Danger, "Couldn't ask the host again", vec![e.clone()]));
        }
        let found: Vec<&str> = st.files.iter().filter(|f| f.exists).map(|f| f.path.as_str()).collect();
        let mut desc = match &st.source {
            Some(src) if found.is_empty() => format!("{} ({}) · no startup files of yours", src.program, src.why),
            Some(src) => format!("{} from {} ({})", found.join(", "), src.program, src.why),
            None => String::new(),
        };
        if let Some(hv) = &st.harvest {
            desc.push_str(&format!(" · checked {} in {} ms", ago(st.now.saturating_sub(hv.at)), hv.ms));
        }
        if !desc.is_empty() {
            s = s.child(kit::hint(ui, desc).px(px(4.)));
        }
        if let Some(p) = &st.problem {
            s = s.child(banner(ui, Tone::Neutral, "Nothing to import from", vec![p.clone()]));
        }
        if let Some(f) = st.failure.as_ref().filter(|f| st.harvest.as_ref().is_none_or(|hv| f.at >= hv.at)) {
            let mut lines = vec![f.message.clone()];
            if !f.stderr.is_empty() {
                lines.push(format!("It printed: {}", f.stderr.lines().take(6).collect::<Vec<_>>().join(" · ")));
            }
            if st.harvest.is_some() {
                lines.push("New shells keep what the check before found.".into());
            }
            s = s.child(banner(ui, Tone::Warning, format!("The last check didn't work ({})", ago(st.now.saturating_sub(f.at))), lines));
        }
        let Some(hv) = &st.harvest else {
            let (icon, title, line) = if d.checking {
                (
                    Icon::Loader,
                    "Running your shell…",
                    "goatty starts it without a terminal and notes what it sets up. Startup files that are slow take a moment.",
                )
            } else {
                (Icon::FileText, "Nothing checked yet", "Check again runs your shell once to see what it sets up.")
            };
            return s.child(card(ui).child(kit::empty(ui, icon, title, line))).into_any_element();
        };
        let all = items(hv, h);
        if all.is_empty() {
            return s
                .child(card(ui).child(kit::empty(
                    ui,
                    Icon::FileText,
                    "Your startup files set nothing up",
                    "No PATH folders or variables beyond what goatty's shell has anyway.",
                )))
                .into_any_element();
        }
        let query = d.filter.read(cx).text().trim().to_lowercase();
        let count = |f: &dyn Fn(&Item) -> bool| all.iter().filter(|i| f(i)).count();
        let n_off = count(&|i| !i.on);
        let chip_for = |show: Show, id: &'static str, label: &'static str, n: usize| {
            chip(
                ui,
                id,
                label,
                Some(n),
                d.show == show,
                self.act(cx, move |v, cx| {
                    v.dotfiles.show = if v.dotfiles.show == show { Show::All } else { show };
                    cx.notify();
                }),
            )
        };
        s = s.child(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(8.))
                .child(div().flex_1().min_w(px(200.)).child(search_field(ui, "import-search", &d.filter, window, cx)))
                .child(chip_for(Show::All, "import-all", "All", all.len()))
                .child(chip_for(Show::Off, "import-off", "Off", n_off)),
        );
        let shown: Vec<&Item> = all
            .iter()
            .filter(|i| match d.show {
                Show::All => true,
                Show::Off => !i.on,
            })
            .filter(|i| {
                query.is_empty() || i.name.to_lowercase().contains(&query) || i.value.iter().any(|v| v.to_lowercase().contains(&query))
            })
            .collect();
        if shown.is_empty() {
            return s
                .child(card(ui).child(kit::empty(ui, Icon::Search, "Nothing matches", "Try part of a name or a value.")))
                .into_any_element();
        }
        let mono = kit::mono(cx);
        let mut list = card(ui);
        for (k, item) in shown.iter().enumerate() {
            if k > 0 {
                list = list.child(divider(ui));
            }
            list = list.child(self.imported_row(ui, item, d.show == Show::Off, &mono, cx));
        }
        s = s.child(list);
        if !hv.stderr.is_empty() {
            s = s.child(
                kit::hint(ui, format!("Your startup printed: {}", hv.stderr.lines().take(4).collect::<Vec<_>>().join(" · "))).px(px(4.)),
            );
        }
        let note = if h.shell.import.enabled {
            "Switched-off entries stay out of new shells. Apply to use your changes."
        } else {
            "This is what turning the import on would bring. Nothing of it is used yet."
        };
        s.child(kit::hint(ui, note).px(px(4.))).into_any_element()
    }

    /// `only_off`: the list shows switched-off entries, so switching this
    /// one on removes its row.
    fn imported_row(&self, ui: Ui, item: &Item, only_off: bool, mono: &SharedString, cx: &Context<'_, Self>) -> AnyElement {
        let id = |what: &str| SharedString::from(format!("import-{what}-{}", item.key));
        let name = item.name.clone();
        let on = item.on;
        let switch = toggle(
            id("on"),
            on,
            ui,
            self.act_window(cx, move |v, window, cx| {
                if only_off {
                    // The row is going: focus waits on the filter that hid it.
                    focus::hand_over(&v.focus, focus::Handover::Control("import-off".into()), window, cx);
                }
                let name = name.clone();
                v.edit_host(cx, move |h| {
                    let list = &mut h.shell.import.skip_vars;
                    if let Some(at) = list.iter().position(|x| *x == name) {
                        list.remove(at);
                    } else {
                        list.push(name);
                        list.sort();
                    }
                });
            }),
        )
        .aria_label(SharedString::from(format!("Import {} {}", item.kind, item.name)));
        div()
            .id(id("row"))
            .flex()
            .items_start()
            .gap(px(14.))
            .min_h(px(48.))
            .px(px(16.))
            .py(px(10.))
            .child(switch)
            .child(
                div()
                    .w(px(150.))
                    .min_w(px(80.))
                    .flex_shrink(1.)
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .child(
                        div()
                            .font_family(mono.clone())
                            .text_size(px(text::BODY))
                            .font_weight(FontWeight::BOLD)
                            .truncate()
                            .text_color(if on { ui.text } else { ui.faint })
                            .child(SharedString::from(item.name.clone())),
                    )
                    .child(div().flex().child(badge(item.kind, Tone::Neutral, ui))),
            )
            .child(div().flex_1().min_w_0().flex().flex_col().gap(px(2.)).children(item.value.iter().map(|line| {
                div()
                    .font_family(mono.clone())
                    .text_size(px(text::LABEL))
                    .text_color(if on { ui.muted } else { ui.faint })
                    .truncate()
                    .child(SharedString::from(if line.is_empty() { "(empty)".to_owned() } else { line.clone() }))
            })))
            .into_any_element()
    }

    /// Where the user's aliases went: copied once on the Aliases page.
    fn aliases_offer(&self, ui: Ui, h: &HostSettings, cx: &Context<'_, Self>) -> AnyElement {
        let offer = self.dotfiles.status.as_ref().and_then(|s| s.harvest.as_ref()).map(|hv| {
            let list = goatty_shell::import::offer_aliases(hv, h);
            (list.iter().filter(|a| a.own).count(), list.len())
        });
        let desc = match offer {
            Some((_, 0)) => "Every alias your shell defines is in goatty already".to_string(),
            Some((0, n)) => format!("{n} from frameworks and plugins you load; pick the ones you want"),
            Some((own, _)) => format!("{own} of your own to copy into goatty's aliases, once"),
            None => "Copy the ones your startup files define into goatty's aliases, once".to_string(),
        };
        section(ui, "Your aliases")
            .child(
                row(ui, "Copy your aliases")
                    .icon(Icon::Link2)
                    .desc(desc)
                    .on_click("import-aliases-go", self.listener_go(cx, Page::Aliases)),
            )
            .into_any_element()
    }

    /// Running the user's files in goatty's shell itself (advanced: the
    /// import covers what most people need).
    fn startup_files_section(&self, ui: Ui, h: &HostSettings, managed: bool, cx: &Context<'_, Self>) -> AnyElement {
        let z = &h.shell.zsh;
        section(ui, "Run your files in goatty's shell")
            .desc(if managed {
                "Advanced: most setups only need the import above"
            } else {
                "Only with goatty's managed zsh"
            })
            .child(row(ui, "Use ~/.zshenv and ~/.zprofile").desc("Your environment: PATH, EDITOR, …").disabled(!managed).control(toggle(
                "user-env",
                z.user_env,
                ui,
                self.host(cx, |h| h.shell.zsh.user_env = !h.shell.zsh.user_env),
            )))
            .child(
                row(ui, "Also run ~/.zshrc")
                    .desc("For functions and key bindings the import can't bring. Frameworks and prompts it loads (oh-my-zsh, Powerlevel10k, zinit, …) are skipped")
                    .disabled(!managed)
                    .control(toggle("user-rc", z.user_rc, ui, self.host(cx, |h| h.shell.zsh.user_rc = !h.shell.zsh.user_rc))),
            )
            .footer("Your own additions for goatty's zsh go in ~/.config/goatty/zshrc.")
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use goatty_shell::import::{Alias, AliasKind, Kind, Source, Var};

    fn harvest() -> Harvest {
        Harvest {
            source: Source { kind: Kind::Zsh, program: "/bin/zsh".into(), why: String::new() },
            at: 0,
            ms: 0,
            vars: vec![Var { name: "EDITOR".into(), value: "nvim".into() }],
            path: vec!["/opt/a".into(), "/usr/bin".into()],
            base_path: vec!["/usr/bin".into()],
            aliases: vec![Alias { name: "ll".into(), value: "ls -l".into(), kind: AliasKind::Regular, own: true }],
            zdotdir: None,
            stderr: String::new(),
            fingerprint: String::new(),
        }
    }

    #[test]
    fn items_follow_the_skip_lists() {
        let mut s = HostSettings::default();
        s.shell.import.skip_vars = vec!["PATH".into()];
        let list = items(&harvest(), &s);
        let names: Vec<_> = list.iter().map(|i| (i.name.as_str(), i.on)).collect();
        assert_eq!(names, [("PATH", false), ("EDITOR", true)], "aliases aren't imported live");
        assert_eq!(list[0].value, ["/opt/a"], "only the folders the startup added");
        assert_eq!(ago(30), "just now");
        assert_eq!(ago(7200), "2 h ago");
    }
}

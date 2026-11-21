//! The install and update review on the Plugins page.
//!
//! An install shows what the plugin will do, starting with "At a glance":
//! a few checks that say where to look (signature, shell code, downloads,
//! network). An update (#244) shows only what changes from the installed
//! version ([`goatty_plugin::changes`]), why it needs a look, and the
//! shell code as a diff; the whole plan stays one click away. The buttons
//! sit at the top and the bottom, so approving never needs a scroll.

use goatty_plugin::Permission;
use goatty_plugin::api::Plan;
use goatty_plugin::changes::{Changes, CodeChange, Kind, ListChange, url_host};
use gpui::{AnyElement, Context, FontWeight, IntoElement, ParentElement, SharedString, Styled, div, prelude::*, px};

use super::kit::{self, card, row, section};
use super::plugins::{perm_icon, size};
use super::{SettingsWindow, plugins::PluginsPage};
use crate::ui::controls::{ButtonKind, Tone, badge, banner, button, button_with, tooltip};
use crate::ui::focus;
use crate::ui::icons::Icon;
use crate::ui::style::{Ui, radius, text};

/// A review hint: an icon tinted by how much it matters, a statement, and
/// what it means.
fn check(ui: Ui, icon: Icon, tone: Tone, title: impl Into<SharedString>, desc: impl Into<SharedString>) -> AnyElement {
    let c = tone.color(ui);
    div()
        .flex()
        .items_start()
        .gap(px(14.))
        .px(px(16.))
        .py(px(12.))
        .child(
            div()
                .flex_shrink_0()
                .size(px(30.))
                .rounded_full()
                .bg(if tone == Tone::Neutral { ui.field } else { c.opacity(0.14) })
                .flex()
                .items_center()
                .justify_center()
                .child(icon.svg().size(px(15.)).text_color(c)),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap(px(2.))
                .min_w_0()
                .flex_1()
                .child(div().text_size(px(text::BODY)).text_color(ui.text).child(title.into()))
                .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child(desc.into())),
        )
        .into_any_element()
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The hosts downloads come from, deduplicated (`github.com, example.org`).
fn hosts<'a>(urls: impl Iterator<Item = &'a str>) -> String {
    let mut out: Vec<String> = Vec::new();
    for u in urls.filter(|u| !u.is_empty()) {
        let h = url_host(u);
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out.join(", ")
}

impl SettingsWindow {
    /// Like [`act`](Self::act), then moves keyboard focus to the page's
    /// first control: the review shown changes or closes.
    fn act_focus_page(
        &self,
        cx: &Context<'_, Self>,
        f: impl Fn(&mut Self, &mut Context<'_, Self>) + 'static,
    ) -> impl Fn(&mut gpui::Window, &mut gpui::App) + 'static {
        let this = cx.entity().downgrade();
        move |window, cx| {
            let _ = this.update(cx, |v, cx| {
                f(v, cx);
                focus::hand_over(&v.focus, focus::Handover::FirstOnPage, window, cx);
            });
        }
    }

    /// The review of `plan`: what changes (an update) or what it does (an
    /// install), between the same actions at the top and the bottom.
    pub(super) fn plan_view(&self, ui: Ui, plan: &Plan, cx: &Context<'_, Self>) -> AnyElement {
        // A reinstall of the same version has nothing to compare.
        let changes = plan.changes().filter(|c| c.from != c.to);
        let mut page = div()
            .flex()
            .flex_col()
            .gap(px(28.))
            // An approved update installing meanwhile (reviews go on while it runs).
            .children(self.plugins.follow.as_ref().map(|f| self.install_progress(ui, f, cx)))
            .children(self.batch_progress(ui, cx))
            .child(self.plan_hero(ui, plan, Some(changes.as_ref()), cx));
        match &changes {
            Some(c) => {
                page = page.child(Self::verdict(ui, c));
                if !plan.warnings.is_empty() {
                    page = page.child(banner(ui, Tone::Warning, "Read this first", plan.warnings.clone()));
                }
                page = page.children(self.change_sections(ui, plan, c, cx));
                page = page.children(self.dependency_reviews(ui, plan, cx));
                let open = self.plugins.show_full_plan;
                let s = section(ui, "Everything it does").child(
                    row(ui, if open { "Hide the whole plan" } else { "Show the whole plan" })
                        .icon(Icon::FileText)
                        .desc(format!("Every permission, download and alias of version {}, not only what changes", plan.version))
                        .on_click(
                            "plan-full",
                            cx.listener(|v, _, _, cx| {
                                v.plugins.show_full_plan = !v.plugins.show_full_plan;
                                cx.notify();
                            }),
                        ),
                );
                page = page.child(s);
                if open {
                    page = page.children(self.plan_sections(ui, plan, cx));
                }
            }
            None => {
                if plan.replaces.as_ref().is_some_and(|old| *old != plan.version) {
                    page = page.child(banner(
                        ui,
                        Tone::Accent,
                        "Couldn't compare with the installed version",
                        vec![
                            "Showing everything this version does. (A goattyd older than this goatty doesn't send what an update changes.)"
                                .into(),
                        ],
                    ));
                }
                page = page.child(Self::at_a_glance(ui, plan));
                if !plan.warnings.is_empty() {
                    page = page.child(banner(ui, Tone::Warning, "Read this first", plan.warnings.clone()));
                }
                page = page.children(self.plan_sections(ui, plan, cx));
                page = page.children(self.dependency_reviews(ui, plan, cx));
            }
        }
        page.child(self.plan_actions(ui, plan, "bottom", cx).justify_end()).into_any_element()
    }

    /// The plugins installed along with `plan`, each with its whole plan.
    fn dependency_reviews(&self, ui: Ui, plan: &Plan, cx: &Context<'_, Self>) -> Vec<AnyElement> {
        if plan.dependencies.is_empty() {
            return Vec::new();
        }
        let names: Vec<String> = plan.dependencies.iter().map(|d| d.name.clone()).collect();
        let mut out = vec![
            banner(
                ui,
                Tone::Accent,
                format!("Also installs {}", names.join(", ")),
                vec![format!(
                    "{} needs {}: installed first, with the same approval. What each does is below.",
                    plan.name,
                    names.join(", ")
                )],
            )
            .into_any_element(),
        ];
        for d in &plan.dependencies {
            out.push(self.plan_hero(ui, d, None, cx));
            out.push(Self::at_a_glance(ui, d));
            out.extend(self.plan_sections(ui, d, cx));
        }
        out
    }

    /// Cancel (Skip in a queue), Skip all, and Install/Update.
    fn plan_actions(&self, ui: Ui, plan: &Plan, at: &str, cx: &Context<'_, Self>) -> gpui::Div {
        let queued = self.plugins.review_position().is_some();
        let busy = self.plugins.follow.is_some() || self.plugins.batch.is_some();
        let verb = match &plan.replaces {
            Some(old) if *old != plan.version => format!("Update to {}", plan.version),
            Some(_) => "Reinstall".into(),
            None => "Install".into(),
        };
        let verb = if busy { format!("{verb} next") } else { verb };
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(8.))
            .when(queued, |d| {
                d.child(button(
                    SharedString::from(format!("plan-skip-all-{at}")),
                    "Skip all",
                    ButtonKind::Ghost,
                    ui,
                    self.act_focus_page(cx, |v, cx| v.skip_all_reviews(cx)),
                ))
            })
            .child(
                button(
                    SharedString::from(format!("plan-cancel-{at}")),
                    if queued { "Skip" } else { "Cancel" },
                    ButtonKind::Ghost,
                    ui,
                    self.act_focus_page(cx, move |v, cx| if queued { v.skip_review(cx) } else { v.back(cx) }),
                )
                .aria_label(SharedString::from(if queued {
                    format!("Skip the {} update", plan.name)
                } else {
                    "Cancel".to_string()
                })),
            )
            .child(button_with(
                SharedString::from(format!("plan-install-{at}")),
                Some(Icon::Download),
                verb,
                ButtonKind::Primary,
                ui,
                self.act_focus_page(cx, |v, cx| v.install_reviewed(cx)),
            ))
    }

    /// A planned plugin's name, version (old → new), origin and summary.
    /// `changes`: `Some` for the reviewed plan itself, which gets the
    /// actions (its changes when it's an update); `None` for a dependency.
    fn plan_hero(&self, ui: Ui, plan: &Plan, changes: Option<Option<&Changes>>, cx: &Context<'_, Self>) -> AnyElement {
        let source = if plan.builtin {
            "Comes with goatty: this installs the tool it needs into goatty's own directory".to_string()
        } else if plan.verified {
            format!("By {} · from the {} registry, signature checked", plan.publisher, plan.source)
        } else {
            format!("By {} · from {}, not from a registry", plan.publisher, plan.source)
        };
        let version = match &plan.replaces {
            Some(old) if *old != plan.version => format!("{old} → {}", plan.version),
            _ => plan.version.clone(),
        };
        let is_update = matches!(changes, Some(Some(_)));
        let position = self.plugins.review_position();
        let actions = changes.is_some().then(|| {
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(12.))
                .flex_wrap()
                .child(kit::hint(ui, Self::position_hint(&self.plugins, position)))
                .child(self.plan_actions(ui, plan, "top", cx).ml_auto())
        });
        card(ui)
            .p(px(20.))
            .gap(px(14.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.))
                    .child(Self::plugin_avatar(ui, goatty_plugin::manifest::Category::Environment, plan.builtin, 52.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .min_w_0()
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(8.))
                                    .child(
                                        div()
                                            .text_size(px(text::HEADING))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(SharedString::from(plan.name.clone())),
                                    )
                                    .child(badge(version, if is_update { Tone::Accent } else { Tone::Neutral }, ui))
                                    .when(plan.verified, |d| {
                                        d.child(
                                            div()
                                                .id(SharedString::from(format!("plan-verified-{}", plan.id)))
                                                .child(Icon::ShieldCheck.svg().size(px(16.)).text_color(ui.success))
                                                .tooltip(tooltip("Signed by its registry")),
                                        )
                                    }),
                            )
                            .child(div().text_size(px(text::SMALL)).text_color(ui.muted).child(SharedString::from(source))),
                    ),
            )
            .when(!plan.summary.trim().is_empty(), |d| {
                d.child(div().text_size(px(text::BODY)).child(SharedString::from(plan.summary.clone())))
            })
            // An update's long description is the same as before: leave it out.
            .when(!is_update && !plan.description.trim().is_empty(), |d| {
                d.child(
                    div().text_size(px(text::SMALL)).text_color(ui.muted).child(SharedString::from(plan.description.trim().to_string())),
                )
            })
            .children(actions.map(|a| div().border_t_1().border_color(ui.divider).pt(px(14.)).child(a)))
            .into_any_element()
    }

    /// "Update 2 of 3 to review", or what happens on approval.
    fn position_hint(page: &PluginsPage, position: Option<(usize, usize)>) -> String {
        let busy = page.follow.is_some() || page.batch.is_some();
        match (position, busy) {
            (Some((n, of)), true) => format!("Review {n} of {of} · approved ones install one after the other"),
            (Some((n, of)), false) => format!("Review {n} of {of} · skipped ones stay as they are"),
            (None, true) => "Starts when the current install is done".into(),
            (None, false) => "Nothing changes on the host until you approve".into(),
        }
    }

    /// Whether the update needs a look, and why.
    fn verdict(ui: Ui, c: &Changes) -> AnyElement {
        let reasons = c.review_reasons();
        if reasons.is_empty() {
            let line = if c.is_empty() {
                "Only the version number changes.".to_string()
            } else {
                "Same permissions and shell code. What else changes is below.".to_string()
            };
            return banner(ui, Tone::Success, "Nothing new to approve", vec![line]).into_any_element();
        }
        banner(ui, Tone::Warning, "Look at this before updating", reasons).into_any_element()
    }

    /// The checks an install review starts with: where to look.
    fn at_a_glance(ui: Ui, plan: &Plan) -> AnyElement {
        let mut s = section(ui, "At a glance");
        s = s.child(if plan.builtin {
            check(ui, Icon::ShieldCheck, Tone::Success, "Comes with goatty", "This only installs the program it needs")
        } else if plan.verified {
            check(
                ui,
                Icon::ShieldCheck,
                Tone::Success,
                format!("Signed by the {} registry", plan.source),
                format!("Published by {}; the signature and every file's checksum were checked", plan.publisher),
            )
        } else {
            check(
                ui,
                Icon::TriangleAlert,
                Tone::Danger,
                "Not signed by any registry",
                format!("From {}: nobody reviewed it. Install it only if you trust whoever wrote it.", plan.source),
            )
        });
        s = s.child(match &plan.shell_code {
            Some((file, code)) => check(
                ui,
                Icon::Code,
                Tone::Warning,
                "Runs its own shell code in every new shell",
                format!("{} in {file}: read it under Shell code below", plural(code.lines().count(), "line", "lines")),
            ),
            None => check(
                ui,
                Icon::CircleCheck,
                Tone::Success,
                "No shell code of its own",
                "goatty writes its aliases and variables itself, quoted",
            ),
        });
        // A zsh plugin in a package runs in every shell just the same.
        let scripts: Vec<&str> = plan.downloads.iter().filter(|d| d.script.is_some()).map(|d| d.package.as_str()).collect();
        if !scripts.is_empty() {
            s = s.child(check(
                ui,
                Icon::Code,
                Tone::Warning,
                "Its programs bring zsh scripts that every new shell sources",
                format!("From {}: they come with the download, not with the plugin's own files", scripts.join(", ")),
            ));
        }
        let fetched: Vec<_> = plan.downloads.iter().filter(|d| d.fetches()).collect();
        s = s.child(if fetched.is_empty() {
            check(
                ui,
                Icon::CircleCheck,
                Tone::Success,
                "Downloads nothing",
                if plan.downloads.is_empty() { "It installs no programs" } else { "The programs it needs are already here" },
            )
        } else {
            let total: u64 = fetched.iter().map(|d| d.size).sum();
            check(
                ui,
                Icon::Download,
                Tone::Neutral,
                format!("Downloads {}, {}", plural(fetched.len(), "program", "programs"), size(total)),
                format!(
                    "From {}, checked against the {} checksums, into goatty's own directory: no sudo",
                    hosts(fetched.iter().map(|d| d.url.as_str())),
                    if plan.verified { "signed" } else { "plugin's own (unsigned)" }
                ),
            )
        });
        for (p, icon, title) in [
            (Permission::Network, Icon::Globe, "May use the network"),
            (Permission::SnippetSources, Icon::Zap, "Runs commands to suggest snippet values"),
        ] {
            if let Some(g) = plan.permissions.iter().find(|g| g.permission == p) {
                let why = if g.reason.is_empty() { g.meaning.clone() } else { g.reason.clone() };
                s = s.child(check(ui, icon, Tone::Warning, title, why));
            }
        }
        if !plan.dependencies.is_empty() {
            let code = plan.dependencies.iter().filter(|d| d.runs_code()).count();
            s = s.child(check(
                ui,
                Icon::Puzzle,
                if code > 0 { Tone::Warning } else { Tone::Neutral },
                format!("Also installs {}", plural(plan.dependencies.len(), "plugin", "plugins")),
                if code > 0 {
                    format!("{} of them run programs or shell code; each is reviewed below", code)
                } else {
                    "Each is reviewed below".to_string()
                },
            ));
        }
        let replaced = plan.warnings.iter().filter(|w| w.contains("is an existing command")).count();
        if replaced > 0 {
            s = s.child(check(
                ui,
                Icon::Link2,
                Tone::Warning,
                format!("Its aliases replace {}", plural(replaced, "existing command", "existing commands")),
                "See Read this first below; each alias can be turned off in its settings",
            ));
        }
        s.into_any_element()
    }

    /// An update's differences, one section per kind, only where something changes.
    fn change_sections(&self, ui: Ui, plan: &Plan, c: &Changes, cx: &Context<'_, Self>) -> Vec<AnyElement> {
        let mono = kit::mono(cx);
        let mut out = Vec::new();

        // Program and alias/variable details show in their own sections.
        let shown = |p: &Permission| match p {
            Permission::Binaries => !c.program_changes.is_empty(),
            Permission::Aliases | Permission::Env | Permission::ConfigFiles => false,
            _ => true,
        };
        let perms: Vec<_> = c.permissions.iter().filter(|p| p.kind != Kind::Changed || shown(&p.granted.permission)).collect();
        if !perms.is_empty() {
            out.push(
                section(ui, "Permissions")
                    .children(perms.into_iter().map(|p| {
                        let g = &p.granted;
                        let (tone, label) = match p.kind {
                            Kind::Added => (Tone::Warning, "New"),
                            Kind::Changed => (Tone::Accent, "Changed"),
                            Kind::Removed => (Tone::Neutral, "Removed"),
                        };
                        let desc = match p.kind {
                            Kind::Added => {
                                let mut d = String::new();
                                if !g.details.is_empty() {
                                    d.push_str(&g.details.join(", "));
                                    d.push_str(". ");
                                }
                                if !g.reason.is_empty() {
                                    d.push_str(&g.reason);
                                    d.push(' ');
                                }
                                d.push_str(&g.meaning);
                                d
                            }
                            Kind::Changed => g.meaning.clone(),
                            Kind::Removed => "The new version doesn't need it".into(),
                        };
                        let r = row(ui, g.title.clone()).icon(perm_icon(g.permission)).desc(desc).control(badge(label, tone, ui));
                        if p.kind == Kind::Changed {
                            r.below(diff_list(ui, &mono, &ListChange { added: p.added.clone(), removed: p.removed.clone() }))
                        } else {
                            r
                        }
                    }))
                    .into_any_element(),
            );
        }

        if !c.packages.is_empty() {
            out.push(
                section(ui, "Programs")
                    .children(c.packages.iter().map(|p| {
                        let from = match &p.host {
                            None => "Already on this host: nothing to download".to_string(),
                            Some(h) if plan.verified => format!("From {h}, checked against the checksum in the signed index"),
                            Some(h) => format!("From {h}, checked against the plugin's own (unsigned) checksum"),
                        };
                        let from = match &p.concern {
                            Some(why) => format!("{why}. {from}"),
                            None => from,
                        };
                        match (&p.from, &p.to) {
                            (Some(a), Some(b)) => {
                                row(ui, if a == b { format!("{} {b}", p.package) } else { format!("{} {a} → {b}", p.package) })
                                    .icon(Icon::Package)
                                    .desc(from)
                                    .control(match &p.concern {
                                        Some(_) => badge("Changed", Tone::Warning, ui),
                                        None => badge(if p.size > 0 { size(p.size) } else { "Skipped".into() }, Tone::Neutral, ui),
                                    })
                            }
                            (None, Some(b)) => row(ui, format!("{} {b}", p.package))
                                .icon(Icon::Package)
                                .desc(format!("New program. {from}"))
                                .control(badge("New", Tone::Warning, ui)),
                            _ => row(ui, p.package.clone()).icon(Icon::Package).desc("goatty's copy is removed").control(badge(
                                "Removed",
                                Tone::Neutral,
                                ui,
                            )),
                        }
                    }))
                    .footer("Into goatty's own directory, no sudo.")
                    .into_any_element(),
            );
        }

        let lists: Vec<(&str, &ListChange)> =
            [("Aliases", &c.aliases), ("Environment variables", &c.env), ("Config files", &c.files), ("Needs", &c.requires)]
                .into_iter()
                .filter(|(_, l)| !l.is_empty())
                .collect();
        if !lists.is_empty() {
            out.push(
                section(ui, "Shell")
                    .children(lists.into_iter().map(|(t, l)| {
                        let desc = match (l.added.len(), l.removed.len()) {
                            (a, 0) => format!("{a} added"),
                            (0, r) => format!("{r} removed"),
                            (a, r) => format!("{a} added, {r} removed"),
                        };
                        row(ui, t).desc(desc).below(diff_list(ui, &mono, l))
                    }))
                    .into_any_element(),
            );
        }

        if let Some(code) = &c.shell_code {
            let desc = if code.dropped {
                format!("The new version no longer runs {}", code.file)
            } else if code.new_code {
                format!("{} runs when every goatty shell starts: new with this version", code.file)
            } else {
                format!("{} runs when every goatty shell starts", code.file)
            };
            out.push(
                section(ui, "Shell code")
                    .desc(desc)
                    .action(div().flex().gap(px(6.)).child(badge(format!("+{}", code.added), Tone::Success, ui)).child(badge(
                        format!("−{}", code.removed),
                        Tone::Danger,
                        ui,
                    )))
                    .child(code_diff(ui, &mono, plan, code))
                    .into_any_element(),
            );
        }
        out
    }

    /// A planned plugin's permissions, downloads, shell additions and code.
    pub(super) fn plan_sections(&self, ui: Ui, plan: &Plan, cx: &Context<'_, Self>) -> Vec<AnyElement> {
        let mono = kit::mono(cx);
        let mut page: Vec<AnyElement> = Vec::new();
        page.push(
            section(ui, "What it may do")
                .children(plan.permissions.iter().map(|g| {
                    let mut desc = String::new();
                    if !g.details.is_empty() {
                        desc.push_str(&g.details.join(", "));
                        desc.push_str(". ");
                    }
                    if !g.reason.is_empty() {
                        desc.push_str(&g.reason);
                        desc.push(' ');
                    }
                    desc.push_str(&g.meaning);
                    row(ui, g.title.clone()).icon(perm_icon(g.permission)).desc(desc)
                }))
                .into_any_element(),
        );
        if !plan.downloads.is_empty() {
            page.push(
                section(ui, "Downloads")
                    .children(plan.downloads.iter().map(|d| {
                        let (desc, control) = match &d.system {
                            Some(p) => (format!("Already installed at {p}: goatty uses that copy"), badge("Skipped", Tone::Success, ui)),
                            None if d.current => ("goatty's copy is already installed".to_string(), badge("Skipped", Tone::Success, ui)),
                            None => (format!("From {}", url_host(&d.url)), badge(size(d.size), Tone::Neutral, ui)),
                        };
                        row(ui, format!("{} {}", d.package, d.version)).icon(Icon::Download).desc(desc).control(control)
                    }))
                    .footer(if plan.verified {
                        "Into goatty's own directory, no sudo. Each download is checked against the checksum in the signed manifest before it's unpacked."
                    } else {
                        "Into goatty's own directory, no sudo. Each download is checked against the plugin's checksum before it's unpacked; nothing signed it."
                    })
                    .into_any_element(),
            );
        }
        // Short items as chips; long ones (alias definitions) as wrapped lines.
        let chips = |items: &[String]| {
            if items.iter().any(|i| i.len() > 32) {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .p(px(10.))
                    .rounded(px(radius::CONTROL))
                    .bg(ui.bg)
                    .border_1()
                    .border_color(ui.divider)
                    .font_family(mono.clone())
                    .text_size(px(text::CAPTION))
                    .children(items.iter().map(|i| div().child(SharedString::from(i.clone()))))
            } else {
                div().flex().flex_wrap().gap(px(6.)).children(items.iter().map(|i| {
                    div()
                        .px(px(8.))
                        .py(px(3.))
                        .rounded(px(radius::SMALL))
                        .bg(ui.field)
                        .font_family(mono.clone())
                        .text_size(px(text::CAPTION))
                        .child(SharedString::from(i.clone()))
                }))
            }
        };
        let adds: Vec<(&str, &Vec<String>)> =
            [("Aliases", &plan.aliases), ("Environment variables", &plan.env), ("Config files", &plan.files), ("Needs", &plan.requires)]
                .into_iter()
                .filter(|(_, v)| !v.is_empty())
                .collect();
        if !adds.is_empty() {
            page.push(
                section(ui, "Adds to your shell").children(adds.into_iter().map(|(t, v)| row(ui, t).below(chips(v)))).into_any_element(),
            );
        }
        if let Some((file, code)) = &plan.shell_code {
            let open = self.plugins.show_code;
            let mut s = section(ui, "Shell code").child(
                row(ui, if open { "Hide the code" } else { "Read the code" })
                    .icon(Icon::Code)
                    .desc(format!("{file} runs when every goatty shell starts · {} lines", code.lines().count()))
                    .on_click(
                        SharedString::from(format!("plan-code-{}", plan.id)),
                        cx.listener(|v, _, _, cx| {
                            v.plugins.show_code = !v.plugins.show_code;
                            cx.notify();
                        }),
                    ),
            );
            if open {
                s = s.child(
                    div()
                        .id(SharedString::from(format!("plan-code-text-{}", plan.id)))
                        .mx(px(16.))
                        .mb(px(16.))
                        .p(px(12.))
                        .rounded(px(radius::CONTROL))
                        .bg(ui.bg)
                        .border_1()
                        .border_color(ui.divider)
                        .font_family(mono.clone())
                        .text_size(px(text::CAPTION))
                        .children(code.lines().map(|l| {
                            div().min_w_0().child(SharedString::from(if l.is_empty() { " ".to_string() } else { l.to_string() }))
                        })),
                );
            }
            page.push(s.into_any_element());
        }
        page
    }
}

/// Added items (`+`, tinted) over removed ones (`−`, muted, struck).
fn diff_list(ui: Ui, mono: &SharedString, l: &ListChange) -> AnyElement {
    let line = |sign: &str, t: &str, added: bool| {
        div()
            .flex()
            .gap(px(8.))
            .px(px(8.))
            .py(px(2.))
            .rounded(px(radius::SMALL))
            .when(added, |d| d.bg(ui.success.opacity(0.10)))
            .child(div().flex_shrink_0().text_color(if added { ui.success } else { ui.danger }).child(SharedString::from(sign.to_string())))
            .child(
                div()
                    .min_w_0()
                    .text_color(if added { ui.text } else { ui.muted })
                    .when(!added, |d| d.line_through())
                    .child(SharedString::from(t.to_string())),
            )
    };
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .font_family(mono.clone())
        .text_size(px(text::CAPTION))
        .children(l.added.iter().map(|t| line("+", t, true)))
        .children(l.removed.iter().map(|t| line("−", t, false)))
        .into_any_element()
}

/// The shell code's changed lines with a little context, hunk by hunk.
fn code_diff(ui: Ui, mono: &SharedString, plan: &Plan, code: &CodeChange) -> AnyElement {
    let mut body = div().flex().flex_col();
    for (i, hunk) in code.hunks.iter().enumerate() {
        if i > 0 {
            let from = hunk.iter().find(|l| l.kind != Some(Kind::Removed)).map_or(0, |l| l.line);
            body = body.child(div().px(px(10.)).py(px(2.)).text_color(ui.faint).child(SharedString::from(format!("⋯ line {from}"))));
        }
        for l in hunk {
            let (sign, bg, fg) = match l.kind {
                Some(Kind::Added) => ("+", Some(ui.success.opacity(0.12)), ui.text),
                Some(Kind::Removed) => ("−", Some(ui.danger.opacity(0.12)), ui.muted),
                _ => (" ", None, ui.muted),
            };
            body = body.child(
                div()
                    .flex()
                    .gap(px(10.))
                    .px(px(10.))
                    .when_some(bg, |d, c| d.bg(c))
                    .child(
                        div()
                            .w(px(32.))
                            .flex_shrink_0()
                            .flex()
                            .justify_end()
                            .text_color(ui.faint)
                            .child(SharedString::from(l.line.to_string())),
                    )
                    .child(div().w(px(10.)).flex_shrink_0().text_color(fg).child(sign))
                    .child(div().flex_1().min_w_0().text_color(fg).child(SharedString::from(if l.text.is_empty() {
                        " ".to_string()
                    } else {
                        l.text.clone()
                    }))),
            );
        }
    }
    div()
        .id(SharedString::from(format!("plan-diff-{}", plan.id)))
        .m(px(12.))
        .py(px(8.))
        .rounded(px(radius::CONTROL))
        .bg(ui.bg)
        .border_1()
        .border_color(ui.divider)
        .font_family(mono.clone())
        .text_size(px(text::CAPTION))
        .child(body)
        .into_any_element()
}

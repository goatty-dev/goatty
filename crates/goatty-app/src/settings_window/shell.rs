//! The host's shell pages: Shell and History (Prompt is in `prompt.rs`).

use goatty_config::HostSettings;
use gpui::{AnyElement, Context, IntoElement, ParentElement, SharedString, Styled, div, px};

use super::kit::{self, group, opt, row, section};
use super::{Page, SettingsWindow, builtin_bool_default};
use crate::ui::controls::{Tone, banner, toggle};
use crate::ui::icons::Icon;
use crate::ui::style::Ui;

/// The built-in plugins the Shell page used to switch, now on Plugins.
const BUILTIN_TOOLS: &[&str] = &["autosuggestions", "syntax-highlighting", "fzf", "fzf-tab", "starship"];

impl SettingsWindow {
    /// Problems loading the host's settings, shown on top of its pages.
    /// `None` in the second place: nothing to edit (yet).
    pub(super) fn host_status(&self, ui: Ui) -> (Vec<AnyElement>, Option<HostSettings>) {
        let mut notes = Vec::new();
        if let Some(e) = &self.host_error {
            notes.push(
                banner(ui, Tone::Danger, format!("Couldn't load the settings of {}", self.host_label()), vec![e.clone()])
                    .into_any_element(),
            );
            return (notes, None);
        }
        if self.host.is_none() {
            notes.push(
                kit::empty(ui, Icon::Loader, "Loading…", format!("Asking {} for its settings", self.host_label())).into_any_element(),
            );
            return (notes, None);
        };
        // Problems with a setting on this page show on its row.
        let (issues, vars) = self.unshown_host_problems(self.page);
        if !issues.is_empty() {
            notes.push(banner(ui, Tone::Warning, "Some settings couldn't be read", issues).into_any_element());
        }
        if !vars.is_empty() {
            notes.push(
                banner(ui, Tone::Neutral, format!("Overridden on {} by {}", self.host_label(), vars.join(", ")), Vec::new())
                    .into_any_element(),
            );
        }
        (notes, self.host_value().cloned())
    }

    pub(super) fn host_page(&self, ui: Ui, sections: impl FnOnce(&HostSettings) -> Vec<AnyElement>) -> AnyElement {
        let (notes, h) = self.host_status(ui);
        let mut page = div().flex().flex_col().gap(px(28.)).children(notes);
        if let Some(h) = h {
            page = page.children(sections(&h));
        }
        page.into_any_element()
    }

    pub(super) fn shell(&self, ui: Ui, cx: &Context<'_, Self>) -> AnyElement {
        self.host_page(ui, |h| {
            let program = h.shell.program.clone();
            let choice = match program.as_deref() {
                None => "auto",
                Some("zsh") => "zsh",
                Some("bash") => "bash",
                Some("fish") => "fish",
                Some(_) => "custom",
            };
            let zsh = matches!(choice, "auto" | "zsh");
            let z = &h.shell.zsh;
            let managed = h.shell.managed;
            let hero = group(ui)
                .child(
                    row(ui, "goatty's managed zsh")
                        .icon(Icon::Zap)
                        .desc(if zsh {
                            "Plugins, prompt, history and shell integration in goatty's tabs. Off: plain zsh with your own config"
                        } else {
                            "Only for zsh: pick zsh or Automatic below to use it"
                        })
                        .disabled(!zsh)
                        .control(toggle("managed", managed && zsh, ui, self.host(cx, |h| h.shell.managed = !h.shell.managed))),
                )
                .child(self.everywhere_row(ui, managed && zsh, cx));
            let this = cx.entity().downgrade();
            let program_section = section(ui, "Program")
                .child(row(ui, "Shell").control(self.select(
                    "program",
                    "Shell",
                    vec![
                        opt("auto", "Automatic").desc("zsh if it's installed, else your login shell"),
                        opt("zsh", "zsh"),
                        opt("bash", "bash"),
                        opt("fish", "fish"),
                        opt("custom", "Other…").desc("A program you name"),
                    ],
                    &choice,
                    cx,
                    move |c: &str, _, cx| {
                        let _ = this.update(cx, |v, cx| {
                            v.edit_host(cx, |h| {
                                h.shell.program = match c {
                                    "auto" => None,
                                    "custom" => h
                                        .shell
                                        .program
                                        .clone()
                                        .filter(|p| !["zsh", "bash", "fish"].contains(&p.as_str()))
                                        .or(Some("/bin/sh".into())),
                                    name => Some(name.into()),
                                }
                            })
                        });
                    },
                )))
                .when_some_child((choice == "custom").then_some(()), |_| {
                    row(ui, "Program to run")
                        .desc("A path or a name on PATH. Enter saves")
                        .below(self.shell_path.clone())
                        .into_any_element()
                });
            let off = !(managed && zsh);
            let imp = &h.shell.import;
            let mut used = Vec::new();
            if imp.enabled {
                used.push("importing what your shell sets up");
            }
            if z.user_env {
                used.push("~/.zshenv and ~/.zprofile");
            }
            if z.user_rc {
                used.push("~/.zshrc");
            }
            let dotfiles = section(ui, "Your dotfiles").child(
                row(ui, "Dotfiles")
                    .icon(Icon::FileText)
                    .desc(if off {
                        "Only with goatty's managed zsh".to_string()
                    } else if used.is_empty() {
                        "PATH folders and variables from your .zshrc or .bashrc · nothing used".to_string()
                    } else {
                        format!("Using {}", used.join(", "))
                    })
                    .on_click("shell-dotfiles", self.listener_go(cx, Page::Dotfiles)),
            );
            let on = BUILTIN_TOOLS.iter().filter(|id| h.plugin_enabled(id)).count();
            let integration = section(ui, "Integration")
                .child(
                    row(ui, "Shell integration")
                        .desc("Marks prompts and commands (OSC 133) and reports the directory (OSC 7)")
                        .disabled(off)
                        .control(toggle(
                            "integration",
                            z.integration,
                            ui,
                            self.host(cx, |h| h.shell.zsh.integration = !h.shell.zsh.integration),
                        )),
                )
                .child(
                    row(ui, "Highlight pasted text")
                        .desc("Shows a paste in reverse video until you press the next key")
                        .disabled(off)
                        .control(toggle(
                            "highlight-paste",
                            z.highlight_paste,
                            ui,
                            self.host(cx, |h| h.shell.zsh.highlight_paste = !h.shell.zsh.highlight_paste),
                        )),
                )
                .child(
                    row(ui, "Built-in plugins")
                        .icon(Icon::Puzzle)
                        .desc(format!(
                            "Autosuggestions, syntax highlighting, fzf, fzf-tab and Starship · {on} of {} on",
                            BUILTIN_TOOLS.len()
                        ))
                        .on_click("shell-plugins", self.listener_go(cx, Page::Plugins)),
                )
                .child(
                    row(ui, "PATH")
                        .icon(Icon::FolderOpen)
                        .desc(match h.shell.path.len() {
                            0 => "Where the shell looks for programs, and where each folder comes from".to_string(),
                            1 => "1 folder added by goatty · where each folder comes from".to_string(),
                            n => format!("{n} folders added by goatty · where each folder comes from"),
                        })
                        .on_click("shell-path", self.listener_go(cx, Page::Path)),
                );
            vec![hero.into_any_element(), program_section.into_any_element(), dotfiles.into_any_element(), integration.into_any_element()]
        })
    }

    // ---- History -----------------------------------------------------------------

    pub(super) fn history(&self, ui: Ui, cx: &Context<'_, Self>) -> AnyElement {
        self.host_page(ui, |h| {
            let hs = h.history.clone();
            let mut sizes: Vec<u32> = vec![1_000, 10_000, 100_000, 1_000_000];
            if !sizes.contains(&hs.size) {
                sizes.push(hs.size);
                sizes.sort_unstable();
            }
            let size_opts = sizes.into_iter().map(|n| opt(n, group_digits(n))).collect();
            let this = cx.entity().downgrade();
            let fzf = h.plugin_enabled("fzf");
            let fzf_tab = h.plugin_enabled("fzf-tab");
            let kept =
                section(ui, "Saving")
                    .child(row(ui, "Commands to keep").desc("In memory and in the history file").control(self.select(
                        "hist-size",
                        "Commands to keep",
                        size_opts,
                        &hs.size,
                        cx,
                        move |n, _, cx| {
                            let _ = this.update(cx, |v, cx| v.edit_host(cx, |h| h.history.size = n));
                        },
                    )))
                    .child(row(ui, "Skip duplicates").desc("Don't save a command that repeats the one before").control(toggle(
                        "dups",
                        hs.ignore_duplicates,
                        ui,
                        self.host(cx, |h| h.history.ignore_duplicates = !h.history.ignore_duplicates),
                    )))
                    .child(
                        row(ui, "Skip commands starting with a space").desc("Handy for one-off commands with secrets in them").control(
                            toggle("space", hs.ignore_space, ui, self.host(cx, |h| h.history.ignore_space = !h.history.ignore_space)),
                        ),
                    )
                    .child(row(ui, "Share between running shells").desc("Commands show up in other tabs right away").control(toggle(
                        "share",
                        hs.share_between_sessions,
                        ui,
                        self.host(cx, |h| h.history.share_between_sessions = !h.history.share_between_sessions),
                    )));
            let search = section(ui, "Search")
                .child(
                    row(ui, "Fuzzy history search")
                        .desc("Ctrl+R searches your history with fzf as you type; Ctrl+T picks files, Alt+C directories")
                        .control(toggle("fzf", fzf, ui, self.toggle_plugin(cx, "fzf"))),
                )
                .child(
                    row(ui, "Fuzzy Tab completion")
                        .desc(if fzf { "Tab opens the matches in fzf (fzf-tab)" } else { "Needs fuzzy history search (fzf)" })
                        .disabled(!fzf)
                        .control(toggle("fzf-tab", fzf_tab, ui, self.toggle_plugin(cx, "fzf-tab"))),
                )
                .child(
                    row(ui, "Previews in Tab completion")
                        .desc(if fzf && fzf_tab {
                            "Directories with eza, files with bat (when installed), processes, variables, git"
                        } else {
                            "Needs fuzzy Tab completion"
                        })
                        .disabled(!(fzf && fzf_tab))
                        .control(toggle(
                            "fzf-tab-previews",
                            h.plugin_bool("fzf-tab", "previews", builtin_bool_default("fzf-tab", "previews")),
                            ui,
                            self.toggle_plugin_bool(cx, "fzf-tab", "previews"),
                        )),
                )
                .child(
                    row(ui, "Atuin")
                        .icon(Icon::Puzzle)
                        .desc("History in a database with each command's directory and exit status, synced between machines if you like: a plugin")
                        .on_click("history-plugins", self.listener_go(cx, Page::Plugins)),
                );
            vec![kept.into_any_element(), search.into_any_element()]
        })
    }
}

/// `100000` → `100 000`.
fn group_digits(n: u32) -> SharedString {
    let s = n.to_string();
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push('\u{2009}');
        }
        out.push(ch);
    }
    out.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digits_are_grouped() {
        assert_eq!(group_digits(1_000).as_ref(), "1\u{2009}000");
        assert_eq!(group_digits(100_000).as_ref(), "100\u{2009}000");
        assert_eq!(group_digits(999).as_ref(), "999");
    }
}

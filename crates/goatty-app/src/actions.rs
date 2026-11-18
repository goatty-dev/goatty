//! Actions and their key bindings.
//!
//! Shortcuts are GPUI actions bound in the `Terminal` key context, not
//! hard-coded key matches, so the settings window can rebind them and menus
//! can trigger them. GPUI dispatches a bound action *before* the view's raw
//! key handler, so anything bound here never reaches the shell.
//!
//! The table of bindable actions and their default keys is data in
//! `goatty_config::keys`; this module maps names to GPUI actions and loads
//! the user's bindings (invalid keystrokes are reported, not fatal).

use std::collections::BTreeMap;
use std::rc::Rc;

use gpui::{Action, App, KeyBinding, KeyBindingContextPredicate, actions};

/// Key context set by the terminal view.
pub const TERMINAL_CONTEXT: &str = "Terminal";

actions!(
    terminal,
    [
        /// Copy the selection to the clipboard.
        Copy,
        /// Copy if there is a selection, otherwise send Ctrl+C to the shell.
        CopyOrInterrupt,
        /// Paste the clipboard (bracketed if the application asked for it).
        Paste,
        /// Scroll back one screen.
        ScrollPageUp,
        /// Scroll forward one screen.
        ScrollPageDown,
        /// Scroll so the previous command's prompt is at the top.
        PreviousPrompt,
        /// Scroll so the next command's prompt is at the top (or to the bottom).
        NextPrompt,
        /// The output viewer on all output, with the search field focused.
        Find,
        /// The output viewer on the last (or running) command's output.
        ViewLastOutput,
        IncreaseFontSize,
        DecreaseFontSize,
        /// Back to the font size from the settings.
        ResetFontSize,
        /// Open (or focus) the settings window.
        OpenSettings,
        /// Show the command palette (or close it).
        ToggleCommandPalette,
        /// The command palette with only the hosts to connect to.
        ConnectToHost,
        /// The snippet picker: find a command by what it does and fill it in (#89).
        OpenSnippets,
        /// The command palette with the active tab's host's sessions (open
        /// or end one without entering it).
        ListSessions,
        /// Ask the host's running managed shells to reload their configuration.
        ReloadShells,
        /// The terminal's menu (what a right click shows), at the cursor.
        OpenContextMenu,
        /// The command typed at the prompt (not run yet) to the clipboard.
        CopyCommandLine,
        /// The command typed at the prompt into a new snippet.
        SaveCommandLine,
        /// `sudo` in front of the typed command, or taken away.
        ToggleSudo,
        /// The typed command in `$VISUAL`/`$EDITOR`.
        EditCommandLine,
        ClearCommandLine,
    ]
);

actions!(
    notifications,
    [
        /// Dismiss every toast in every window.
        DismissNotifications,
        /// Press the main (first) button of the window's newest toast that has one.
        PressNotificationButton,
        /// List the recent notifications (or close the list).
        ShowRecentNotifications,
    ]
);

actions!(
    workspace,
    [
        NewTab,
        /// Close the focused pane (the tab with its last pane).
        ClosePane,
        CloseTab,
        NextTab,
        PreviousTab,
        MoveTabLeft,
        MoveTabRight,
        SelectTab1,
        SelectTab2,
        SelectTab3,
        SelectTab4,
        SelectTab5,
        SelectTab6,
        SelectTab7,
        SelectTab8,
        SelectLastTab,
        RenameTab,
        /// A new tab on the same host, in the same directory.
        DuplicateTab,
        CloseOtherTabs,
        /// Fold the vertical tab sidebar into a rail of icons, or unfold it.
        ToggleTabSidebar,
        /// Switch `tabs.style` between vertical and horizontal.
        ToggleVerticalTabs,
        /// The next host's tabs (the one last active there).
        NextHost,
        PreviousHost,
        /// Fold every other host's group of tabs.
        ShowOnlyThisHost,
        /// Unfold every host's group of tabs.
        ShowAllHosts,
        SplitRight,
        SplitDown,
        SplitLeft,
        SplitUp,
        FocusPaneLeft,
        FocusPaneRight,
        FocusPaneUp,
        FocusPaneDown,
        ResizePaneLeft,
        ResizePaneRight,
        ResizePaneUp,
        ResizePaneDown,
        /// Let the focused pane fill the tab, or go back to the split.
        TogglePaneZoom,
        EqualizePanes,
        SwapPaneLeft,
        SwapPaneRight,
        SwapPaneUp,
        SwapPaneDown,
        MovePaneToNewTab,
        /// Ask which tab (or a new one) the focused pane goes to.
        MovePaneToTab,
        /// Ask which other tab joins the active one, beside its panes.
        MergeTab,
        /// Every pane of the tab one place on (the last one first).
        RotatePanes,
        /// Every pane of the tab one place back.
        RotatePanesBack,
    ]
);

/// Run a plugin's palette command (`plugin.<plugin>.<command>`), bound as a
/// user shortcut.
#[derive(Clone, PartialEq, Eq, Debug, gpui::Action)]
#[action(namespace = plugin, no_json)]
pub struct RunPluginCommand {
    pub id: String,
}

/// The GPUI action for a settings name (`goatty_config::keys::ACTIONS`, or
/// `plugin.<plugin>.<command>`).
pub fn action_by_name(name: &str) -> Option<Box<dyn Action>> {
    Some(match name {
        "copy" => Box::new(Copy),
        "copy_or_interrupt" => Box::new(CopyOrInterrupt),
        "paste" => Box::new(Paste),
        "scroll_page_up" => Box::new(ScrollPageUp),
        "scroll_page_down" => Box::new(ScrollPageDown),
        "previous_prompt" => Box::new(PreviousPrompt),
        "next_prompt" => Box::new(NextPrompt),
        "find" => Box::new(Find),
        "view_last_output" => Box::new(ViewLastOutput),
        "increase_font_size" => Box::new(IncreaseFontSize),
        "decrease_font_size" => Box::new(DecreaseFontSize),
        "reset_font_size" => Box::new(ResetFontSize),
        "open_settings" => Box::new(OpenSettings),
        "command_palette" => Box::new(ToggleCommandPalette),
        "connect_to_host" => Box::new(ConnectToHost),
        "snippets" => Box::new(OpenSnippets),
        "list_sessions" => Box::new(ListSessions),
        "reload_shells" => Box::new(ReloadShells),
        "context_menu" => Box::new(OpenContextMenu),
        "copy_command_line" => Box::new(CopyCommandLine),
        "save_command_line" => Box::new(SaveCommandLine),
        "toggle_sudo" => Box::new(ToggleSudo),
        "edit_command_line" => Box::new(EditCommandLine),
        "clear_command_line" => Box::new(ClearCommandLine),
        "dismiss_notifications" => Box::new(DismissNotifications),
        "press_notification_button" => Box::new(PressNotificationButton),
        "show_recent_notifications" => Box::new(ShowRecentNotifications),
        "new_tab" => Box::new(NewTab),
        "close_pane" => Box::new(ClosePane),
        "close_tab" => Box::new(CloseTab),
        "next_tab" => Box::new(NextTab),
        "previous_tab" => Box::new(PreviousTab),
        "move_tab_left" => Box::new(MoveTabLeft),
        "move_tab_right" => Box::new(MoveTabRight),
        "select_tab_1" => Box::new(SelectTab1),
        "select_tab_2" => Box::new(SelectTab2),
        "select_tab_3" => Box::new(SelectTab3),
        "select_tab_4" => Box::new(SelectTab4),
        "select_tab_5" => Box::new(SelectTab5),
        "select_tab_6" => Box::new(SelectTab6),
        "select_tab_7" => Box::new(SelectTab7),
        "select_tab_8" => Box::new(SelectTab8),
        "select_last_tab" => Box::new(SelectLastTab),
        "rename_tab" => Box::new(RenameTab),
        "duplicate_tab" => Box::new(DuplicateTab),
        "close_other_tabs" => Box::new(CloseOtherTabs),
        "toggle_tab_sidebar" => Box::new(ToggleTabSidebar),
        "toggle_vertical_tabs" => Box::new(ToggleVerticalTabs),
        "next_host" => Box::new(NextHost),
        "previous_host" => Box::new(PreviousHost),
        "show_only_this_host" => Box::new(ShowOnlyThisHost),
        "show_all_hosts" => Box::new(ShowAllHosts),
        "split_right" => Box::new(SplitRight),
        "split_down" => Box::new(SplitDown),
        "split_left" => Box::new(SplitLeft),
        "split_up" => Box::new(SplitUp),
        "focus_pane_left" => Box::new(FocusPaneLeft),
        "focus_pane_right" => Box::new(FocusPaneRight),
        "focus_pane_up" => Box::new(FocusPaneUp),
        "focus_pane_down" => Box::new(FocusPaneDown),
        "resize_pane_left" => Box::new(ResizePaneLeft),
        "resize_pane_right" => Box::new(ResizePaneRight),
        "resize_pane_up" => Box::new(ResizePaneUp),
        "resize_pane_down" => Box::new(ResizePaneDown),
        "toggle_pane_zoom" => Box::new(TogglePaneZoom),
        "equalize_panes" => Box::new(EqualizePanes),
        "swap_pane_left" => Box::new(SwapPaneLeft),
        "swap_pane_right" => Box::new(SwapPaneRight),
        "swap_pane_up" => Box::new(SwapPaneUp),
        "swap_pane_down" => Box::new(SwapPaneDown),
        "move_pane_to_new_tab" => Box::new(MovePaneToNewTab),
        "move_pane_to_tab" => Box::new(MovePaneToTab),
        "merge_tab" => Box::new(MergeTab),
        "rotate_panes" => Box::new(RotatePanes),
        "rotate_panes_back" => Box::new(RotatePanesBack),
        _ => {
            return goatty_config::keys::parse_plugin_action(name)
                .map(|_| Box::new(RunPluginCommand { id: name.to_string() }) as Box<dyn Action>);
        }
    })
}

/// Terminal bindings for the given overrides, plus problems found.
pub fn terminal_bindings(overrides: &BTreeMap<String, Option<String>>, cx: &App) -> (Vec<KeyBinding>, Vec<String>) {
    let ctx = KeyBindingContextPredicate::parse(TERMINAL_CONTEXT).ok().map(Rc::new);
    let mut bindings = Vec::new();
    let mut errors = Vec::new();
    for (key, name) in goatty_config::keys::effective_bindings(overrides) {
        let Some(action) = action_by_name(&name) else {
            errors.push(format!("keybindings.{key}: unknown action {name:?}"));
            continue;
        };
        match KeyBinding::load(&key, action, ctx.clone(), false, None, cx) {
            Ok(b) => bindings.push(b),
            Err(e) => errors.push(format!("keybindings.{key}: {e}")),
        }
    }
    (bindings, errors)
}

/// Replace every key binding: the terminal's (from settings) plus the
/// fixed ones of goatty's own UI. Returns problems with user bindings.
pub fn apply_keybindings(overrides: &BTreeMap<String, Option<String>>, cx: &mut App) -> Vec<String> {
    let (terminal, errors) = terminal_bindings(overrides, cx);
    for e in &errors {
        log::warn!("{e}");
    }
    cx.clear_key_bindings();
    cx.bind_keys(terminal);
    cx.bind_keys(crate::ui::text_input::bindings());
    cx.bind_keys(crate::ui::color_picker::bindings());
    cx.bind_keys(crate::output_view::bindings());
    cx.bind_keys(crate::settings_window::bindings());
    cx.bind_keys(crate::welcome::bindings());
    cx.bind_keys(crate::paste_confirm::bindings());
    cx.bind_keys(crate::close_confirm::bindings());
    let palette_keys = goatty_config::keys::keys_for("command_palette", overrides);
    let palette = crate::command_palette::bindings(&palette_keys, cx);
    cx.bind_keys(palette);
    let snippet_keys = goatty_config::keys::keys_for("snippets", overrides);
    let snippets = crate::snippets::picker::bindings(&snippet_keys, cx);
    cx.bind_keys(snippets);
    let recent_keys = goatty_config::keys::keys_for("show_recent_notifications", overrides);
    let recent = crate::toast::recent::bindings(&recent_keys, cx);
    cx.bind_keys(recent);
    cx.bind_keys(crate::settings_window::notification_bindings(overrides, cx));
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_has_a_gpui_action() {
        for a in goatty_config::keys::ACTIONS {
            assert!(action_by_name(a.name).is_some(), "{}", a.name);
        }
    }

    #[test]
    fn default_keys_parse() {
        for (key, _) in goatty_config::keys::DEFAULT_BINDINGS {
            assert!(gpui::Keystroke::parse(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn plugin_action_resolution() {
        let act = action_by_name("plugin.docker.exec").expect("valid plugin action resolves");
        assert_eq!(act.name(), "plugin::RunPluginCommand");
        let run = act.as_any().downcast_ref::<RunPluginCommand>().expect("downcasts to RunPluginCommand");
        assert_eq!(run.id, "plugin.docker.exec");

        assert!(action_by_name("plugin.d.exec").is_none());
        assert!(action_by_name("plugin.docker.Bad").is_none());
        assert!(action_by_name("unknown_action").is_none());
    }
}

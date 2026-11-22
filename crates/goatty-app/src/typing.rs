//! Which pane goatty may type a command into: the focused one, if it's on
//! the host the command is for and a shell is waiting there. Otherwise
//! the user gets the command to copy instead. Plugins' palette commands
//! (`plugin_commands`) and snippets (`snippets`) both go through
//! [`target`].

use gpui::{App, ClipboardItem, Entity, Window};

use crate::settings_window::HostLink;
use crate::terminal::TerminalView;
use crate::toast::{self, Toast};
use crate::workspace::Workspace;

/// What's being typed, for the messages.
#[derive(Debug, Clone, Copy)]
pub enum What {
    /// A plugin's palette command (run right away).
    Command,
    /// A snippet (put on the prompt, not run).
    Snippet,
}

impl What {
    fn noun(self) -> &'static str {
        match self {
            What::Command => "command",
            What::Snippet => "snippet",
        }
    }

    fn key(self) -> &'static str {
        match self {
            What::Command => "plugin.command",
            What::Snippet => "snippet.insert",
        }
    }
}

/// The focused pane to type `line` into, when it's on `origin`'s host and
/// at a prompt; `None` after telling the user why, with a button that
/// copies the line. Call it deferred, outside the workspace's update.
pub fn target(origin: &HostLink, line: &str, what: What, window: &mut Window, cx: &mut App) -> Option<Entity<TerminalView>> {
    target_opt(Some(origin), line, what, window, cx)
}

/// Like [`target`], for a client-side custom command that works on whichever
/// host the focused pane is on.
pub fn target_focused(line: &str, what: What, window: &mut Window, cx: &mut App) -> Option<Entity<TerminalView>> {
    target_opt(None, line, what, window, cx)
}

fn target_opt(origin: Option<&HostLink>, line: &str, what: What, window: &mut Window, cx: &mut App) -> Option<Entity<TerminalView>> {
    let ws = window.root::<Workspace>().flatten();
    let found = ws.as_ref().map(|ws| (ws.read(cx).focused_terminal(), ws.read(cx).active_host_link()));
    let copy = line.to_string();
    let refuse = |title: String, body: String, window: &mut Window, cx: &mut App| {
        let t = Toast::warning(title)
            .body(body)
            .action(format!("Copy the {}", what.noun()), move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone())))
            .key(what.key());
        toast::show_in(window, cx, t);
    };
    let Some((Some(view), host)) = found else {
        refuse(format!("No terminal to put the {} in", what.noun()), line.to_string(), window, cx);
        return None;
    };
    // Listed for another host (the tab changed meanwhile).
    if let Some(origin) = origin
        && !host.is_some_and(|h| h.same(origin))
    {
        refuse(
            format!("This {} is from {}", what.noun(), origin.label()),
            format!("The active tab is on another host. Use it in a tab on {}: {line}", origin.label()),
            window,
            cx,
        );
        return None;
    }
    let v = view.read(cx);
    let busy = match v.process() {
        Some(p) if !crate::workspace::icons::is_shell(p) => Some(p.to_string()),
        // Shell integration knows a command runs even when the program isn't known.
        _ if v.session().terminal.command_running() => Some("A command".to_string()),
        _ => None,
    };
    if let Some(program) = busy {
        refuse(format!("{program} is running in this pane"), format!("Use it at a prompt: {line}"), window, cx);
        return None;
    }
    Some(view)
}

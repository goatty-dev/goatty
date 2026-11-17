//! Notifications about panes (#205): a coding agent waiting for the user,
//! done or failed (OSC 7501), and programs asking for one (OSC 9 / 777).
//!
//! Two independent settings (`notifications.in_app`, `.system`) say when
//! each kind goes out: never, while goatty's window isn't focused, while
//! the pane isn't on screen, or always. `Workspace` decides per pane and
//! keeps one in-app toast per pane (keyed, so a new state replaces the
//! old one in place; looking at the pane dismisses it). Desktop
//! notifications carry a tag naming the window and pane; clicking one
//! brings that pane forward ([`init`]).
//!
//! Text from programs is plain text: control, bidi and zero-width
//! characters are dropped and long text is shortened.

use goatty_config::{NotificationSettings, NotifyWhen};
use goatty_term::StatusState;
use gpui::{App, SystemNotification, SystemNotificationResponse, Window};

use super::{AgentStatus, shorten};
use crate::toast::Toast;
use crate::workspace::Workspace;

/// Something a pane has to tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notice {
    /// Its agent waits for the user, finished or failed.
    Agent(AgentStatus),
    /// Its program asked for a notification (OSC 9 / 777).
    Program { title: String, body: String },
}

/// The longest body a notification shows.
const MAX_BODY: usize = 240;
const MAX_TITLE: usize = 80;
/// The tag prefix of goatty's pane notifications.
const TAG: &str = "goatty-pane";

impl Notice {
    /// A program's notification, its text cleaned; `None` when nothing's left.
    pub fn program(title: &str, body: &str) -> Option<Notice> {
        let (title, body) = (clean(title, MAX_TITLE), clean(body, MAX_BODY));
        (!title.is_empty() || !body.is_empty()).then_some(Notice::Program { title, body })
    }

    /// Do the settings want this kind told at all?
    pub fn wanted(&self, s: &NotificationSettings) -> bool {
        match self {
            Notice::Agent(a) if a.state == StatusState::Blocked => s.agent_waiting,
            Notice::Agent(_) => s.agent_done,
            Notice::Program { .. } => s.programs,
        }
    }

    pub fn is_agent(&self) -> bool {
        matches!(self, Notice::Agent(_))
    }

    /// The headline and the details; `place` names the pane (its tab's
    /// label, which a program may have set: cleaned like its text).
    pub fn text(&self, place: &str) -> (String, String) {
        let place = clean(place, MAX_TITLE);
        match self {
            Notice::Agent(a) => {
                let who = a.who();
                let title = match (a.state, a.kind) {
                    (StatusState::Blocked, Some(goatty_term::BlockedKind::Permission)) => format!("{who} needs your permission"),
                    (StatusState::Blocked, Some(goatty_term::BlockedKind::Question)) => format!("{who} has a question"),
                    (StatusState::Blocked, Some(goatty_term::BlockedKind::Auth)) => format!("{who} needs you to sign in"),
                    (StatusState::Blocked, None) => format!("{who} is waiting for you"),
                    (StatusState::Error, _) => format!("{who} failed"),
                    _ => format!("{who} is done"),
                };
                let body = match a.details(MAX_BODY) {
                    Some(d) => format!("{d}\nIn {place}"),
                    None => format!("In {place}"),
                };
                (title, body)
            }
            Notice::Program { title, body } if title.is_empty() => (place, body.clone()),
            Notice::Program { title, body } => (title.clone(), format!("{body}\nIn {place}").trim().to_string()),
        }
    }

    /// The in-app toast; `show` brings the pane forward.
    pub fn toast(&self, place: &str, key: String, show: impl Fn(&mut Window, &mut App) + 'static) -> Toast {
        let (title, body) = self.text(place);
        let toast = match self {
            Notice::Agent(a) => match a.state {
                // Stays until it's dealt with (looking at the pane dismisses it).
                StatusState::Blocked => Toast::warning(title).sticky(),
                StatusState::Error => Toast::error(title),
                _ => Toast::success(title),
            },
            Notice::Program { .. } => Toast::info(title),
        };
        toast.body(body).key(key).action("Show", show)
    }

    /// The desktop notification for pane `pane` of window `window`.
    pub fn system(&self, place: &str, window: u64, pane: u64) -> SystemNotification {
        let (title, body) = self.text(place);
        SystemNotification { tag: tag(window, pane).into(), title: title.into(), body: body.into(), actions: Vec::new() }
    }
}

/// Should a notification go out under `when`, with goatty's window focused
/// or not and the pane on screen or not?
pub fn applies(when: NotifyWhen, window_active: bool, pane_visible: bool) -> bool {
    match when {
        NotifyWhen::Never => false,
        NotifyWhen::Unfocused => !window_active,
        NotifyWhen::Background => !(window_active && pane_visible),
        NotifyWhen::Always => true,
    }
}

/// A desktop notification's tag: which window and pane it's about.
pub fn tag(window: u64, pane: u64) -> String {
    format!("{TAG}:{window}:{pane}")
}

fn parse_tag(tag: &str) -> Option<(u64, u64)> {
    let mut parts = tag.strip_prefix(TAG)?.strip_prefix(':')?.split(':');
    let window = parts.next()?.parse().ok()?;
    let pane = parts.next()?.parse().ok()?;
    parts.next().is_none().then_some((window, pane))
}

/// Name goatty to the desktop's notification center, and bring a pane
/// forward when its notification is clicked.
pub fn init(cx: &mut App) {
    cx.set_app_identity("dev.goatty.goatty", "goatty");
    cx.on_system_notification_response(respond);
}

fn respond(response: SystemNotificationResponse, cx: &mut App) {
    let Some((window_id, pane)) = parse_tag(&response.tag) else { return };
    let Some(handle) = cx.windows().into_iter().find(|w| w.window_id().as_u64() == window_id) else { return };
    let Some(handle) = handle.downcast::<Workspace>() else { return };
    cx.activate(true);
    let _ = handle.update(cx, |workspace, window, cx| {
        window.activate_window();
        workspace.show_pane(pane, window, cx);
    });
}

/// `text` without control, bidi or zero-width characters, on one line,
/// shortened to `max` characters.
fn clean(text: &str, max: usize) -> String {
    let kept: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .filter(|&c| !matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{2069}' | '\u{061c}' | '\u{feff}'))
        .collect();
    shorten(&kept, max)
}

#[cfg(test)]
mod tests {
    use goatty_term::BlockedKind;

    use super::*;
    use crate::agent::{Agent, Identity};

    fn agent(state: StatusState, kind: Option<BlockedKind>) -> Notice {
        Notice::Agent(AgentStatus {
            who: Some(Identity::Known(Agent::ClaudeCode)),
            id: String::new(),
            state,
            kind,
            progress: None,
            title: Some("Bash".into()),
            msg: Some("cargo test".into()),
            seq: 1,
        })
    }

    #[test]
    fn when_notifications_go_out() {
        use NotifyWhen::*;
        // (window active, pane visible)
        let cases = [(true, true), (true, false), (false, true), (false, false)];
        let table = |when| cases.map(|(w, p)| applies(when, w, p));
        assert_eq!(table(Never), [false; 4]);
        assert_eq!(table(Unfocused), [false, false, true, true]);
        assert_eq!(table(Background), [false, true, true, true]);
        assert_eq!(table(Always), [true; 4]);
    }

    #[test]
    fn texts_say_who_what_and_where() {
        let (t, b) = agent(StatusState::Blocked, Some(BlockedKind::Permission)).text("goatty");
        assert_eq!(t, "Claude Code needs your permission");
        assert_eq!(b, "Bash: cargo test\nIn goatty");
        assert_eq!(agent(StatusState::Done, None).text("x").0, "Claude Code is done");
        assert_eq!(agent(StatusState::Error, None).text("x").0, "Claude Code failed");
        let p = Notice::program("", "Build \u{202e}finished\n").unwrap();
        assert_eq!(p.text("make"), ("make".into(), "Build finished".into()));
        assert_eq!(p.text("evil\u{202e}gnp.exe"), ("evilgnp.exe".into(), "Build finished".into()), "a title a program set is cleaned too");
        assert_eq!(agent(StatusState::Done, None).text("a\u{200b}b").1, "Bash: cargo test\nIn ab");
        assert_eq!(Notice::program(" ", "\u{7}"), None);
    }

    #[test]
    fn settings_pick_the_kinds() {
        let mut s = NotificationSettings::default();
        assert!(agent(StatusState::Blocked, None).wanted(&s));
        s.agent_waiting = false;
        assert!(!agent(StatusState::Blocked, None).wanted(&s));
        assert!(agent(StatusState::Done, None).wanted(&s));
        s.agent_done = false;
        assert!(!agent(StatusState::Error, None).wanted(&s));
        assert!(Notice::program("t", "b").unwrap().wanted(&s));
    }

    #[test]
    fn tags_name_window_and_pane() {
        assert_eq!(parse_tag(&tag(7, 42)), Some((7, 42)));
        assert_eq!(parse_tag("goatty-pane:7"), None);
        assert_eq!(parse_tag("other:7:42"), None);
        assert_eq!(parse_tag("goatty-pane:7:42:1"), None);
    }
}

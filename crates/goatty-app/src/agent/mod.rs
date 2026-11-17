//! Coding agents (#205): which one runs in a pane, its logo, and what it
//! says it's doing.
//!
//! A pane's agent is known by the `app` its OSC 7501 reports carry (which
//! works over plain SSH and whatever runtime the agent runs on), else by
//! the pane's foreground program (`goatty_term::process` names agents that
//! run under `node` or `python` by their package). Known agents get their
//! own logo (`assets/agents/`, drawn in their colors or tinted to the
//! theme when the mark is black); any other program that reports a status
//! gets a generic one.
//!
//! [`AgentStatus`] is what tabs show: the record that deserves the most
//! attention (blocked, error, working, done), with `done` and `error`
//! hidden once the user saw them. `notify` turns changes into
//! notifications.

pub mod notify;

use goatty_term::{BlockedKind, ProgramStatus, StatusRecord, StatusState};
use gpui::{AnyElement, Hsla, IntoElement, Styled, img, px, svg};

use crate::ui::icons::Icon;
use crate::workspace::icons::is_shell;

/// The coding agents goatty knows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Agent {
    ClaudeCode,
    Codex,
    OpenCode,
    Antigravity,
    Gemini,
    Copilot,
    Cursor,
    Goose,
    Pi,
    Amp,
    Qwen,
    Aider,
}

/// How an agent's logo is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Logo {
    /// In its own colors (an image).
    Color(&'static str),
    /// A black mark, tinted with the text color (so it shows on dark themes).
    Mono(&'static str),
    /// No logo of its own: one of goatty's icons.
    Glyph(Icon),
}

/// The logo files, by asset path.
const LOGOS: &[(&str, &[u8])] = &[
    ("agents/claude-code.svg", include_bytes!("../../assets/agents/claude-code.svg")),
    ("agents/codex.svg", include_bytes!("../../assets/agents/codex.svg")),
    ("agents/opencode.svg", include_bytes!("../../assets/agents/opencode.svg")),
    ("agents/antigravity.svg", include_bytes!("../../assets/agents/antigravity.svg")),
    ("agents/gemini-cli.svg", include_bytes!("../../assets/agents/gemini-cli.svg")),
    ("agents/copilot.svg", include_bytes!("../../assets/agents/copilot.svg")),
    ("agents/cursor.svg", include_bytes!("../../assets/agents/cursor.svg")),
    ("agents/goose.svg", include_bytes!("../../assets/agents/goose.svg")),
    ("agents/pi.svg", include_bytes!("../../assets/agents/pi.svg")),
    ("agents/amp.svg", include_bytes!("../../assets/agents/amp.svg")),
    ("agents/qwen.svg", include_bytes!("../../assets/agents/qwen.svg")),
];

/// A logo's file, for `ui::icons::Assets`.
pub(crate) fn logo_bytes(path: &str) -> Option<&'static [u8]> {
    LOGOS.iter().find(|(p, _)| *p == path).map(|(_, bytes)| *bytes)
}

impl Agent {
    pub const ALL: &[Agent] = &[
        Agent::ClaudeCode,
        Agent::Codex,
        Agent::OpenCode,
        Agent::Antigravity,
        Agent::Gemini,
        Agent::Copilot,
        Agent::Cursor,
        Agent::Goose,
        Agent::Pi,
        Agent::Amp,
        Agent::Qwen,
        Agent::Aider,
    ];

    /// The agent an OSC 7501 `app` names.
    pub fn from_app(app: &str) -> Option<Agent> {
        Some(match app.to_ascii_lowercase().as_str() {
            "claude-code" | "claude" | "claudecode" => Agent::ClaudeCode,
            "codex" | "codex-cli" | "openai-codex" => Agent::Codex,
            "opencode" => Agent::OpenCode,
            "agy" | "antigravity" | "antigravity-cli" => Agent::Antigravity,
            "gemini" | "gemini-cli" => Agent::Gemini,
            "copilot" | "copilot-cli" | "github-copilot" | "gh-copilot" => Agent::Copilot,
            "cursor-agent" | "cursor" => Agent::Cursor,
            "goose" => Agent::Goose,
            "pi" | "pi-coding-agent" => Agent::Pi,
            "amp" => Agent::Amp,
            "qwen" | "qwen-code" => Agent::Qwen,
            "aider" => Agent::Aider,
            _ => return None,
        })
    }

    /// The agent a foreground program is.
    pub fn from_process(name: &str) -> Option<Agent> {
        Some(match name {
            "claude" => Agent::ClaudeCode,
            "codex" => Agent::Codex,
            "opencode" => Agent::OpenCode,
            "agy" => Agent::Antigravity,
            "gemini" => Agent::Gemini,
            "copilot" => Agent::Copilot,
            "cursor-agent" => Agent::Cursor,
            "goose" => Agent::Goose,
            "pi" => Agent::Pi,
            "amp" => Agent::Amp,
            "qwen" => Agent::Qwen,
            "aider" => Agent::Aider,
            _ => return None,
        })
    }

    /// What people call it.
    pub fn name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::Codex => "Codex",
            Agent::OpenCode => "OpenCode",
            Agent::Antigravity => "Antigravity",
            Agent::Gemini => "Gemini CLI",
            Agent::Copilot => "Copilot",
            Agent::Cursor => "Cursor Agent",
            Agent::Goose => "Goose",
            Agent::Pi => "Pi",
            Agent::Amp => "Amp",
            Agent::Qwen => "Qwen Code",
            Agent::Aider => "Aider",
        }
    }

    pub fn logo(self) -> Logo {
        match self {
            Agent::ClaudeCode => Logo::Color("agents/claude-code.svg"),
            Agent::Codex => Logo::Color("agents/codex.svg"),
            Agent::OpenCode => Logo::Mono("agents/opencode.svg"),
            Agent::Antigravity => Logo::Color("agents/antigravity.svg"),
            Agent::Gemini => Logo::Color("agents/gemini-cli.svg"),
            Agent::Copilot => Logo::Color("agents/copilot.svg"),
            Agent::Cursor => Logo::Mono("agents/cursor.svg"),
            Agent::Goose => Logo::Mono("agents/goose.svg"),
            Agent::Pi => Logo::Mono("agents/pi.svg"),
            Agent::Amp => Logo::Color("agents/amp.svg"),
            Agent::Qwen => Logo::Color("agents/qwen.svg"),
            Agent::Aider => Logo::Glyph(Icon::Bot),
        }
    }
}

impl Logo {
    /// The asset it's drawn from, unless it's one of goatty's icons.
    pub fn path(self) -> Option<&'static str> {
        match self {
            Logo::Color(p) | Logo::Mono(p) => Some(p),
            Logo::Glyph(_) => None,
        }
    }

    /// The logo, `size` px square; `tint` colors the marks that take one.
    pub fn element(self, size: f32, tint: Hsla) -> AnyElement {
        match self {
            Logo::Color(path) => img(path).flex_none().size(px(size)).into_any_element(),
            Logo::Mono(path) => svg().path(path).flex_none().size(px(size)).text_color(tint).into_any_element(),
            Logo::Glyph(icon) => icon.svg().size(px(size)).text_color(tint).into_any_element(),
        }
    }
}

/// Who runs in a pane, for its icon and its notifications.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Identity {
    Known(Agent),
    /// A program that reports its status but goatty doesn't know (its
    /// OSC 7501 `app`).
    Other(String),
}

impl Identity {
    /// From what the pane's programs reported and its foreground program.
    /// A finished agent's `done`/`error` record outlives it; once the shell
    /// is back in the foreground, the pane is the shell's again.
    pub fn of(status: &ProgramStatus, process: Option<&str>) -> Option<Identity> {
        let live = status.records.iter().any(|r| !matches!(r.state, StatusState::Done | StatusState::Error));
        let shell = process.is_some_and(is_shell);
        if let Some(app) = status.app().filter(|_| live || !shell) {
            return Some(Agent::from_app(app).map_or_else(|| Identity::Other(app.to_string()), Identity::Known));
        }
        process.and_then(Agent::from_process).map(Identity::Known)
    }

    pub fn name(&self) -> &str {
        match self {
            Identity::Known(a) => a.name(),
            Identity::Other(app) => app,
        }
    }

    pub fn logo(&self) -> Logo {
        match self {
            Identity::Known(a) => a.logo(),
            Identity::Other(_) => Logo::Glyph(Icon::Bot),
        }
    }
}

/// What a pane's agent is doing: the record that deserves the most
/// attention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentStatus {
    pub who: Option<Identity>,
    pub id: String,
    pub state: StatusState,
    pub kind: Option<BlockedKind>,
    pub progress: Option<u8>,
    pub title: Option<String>,
    pub msg: Option<String>,
    pub seq: u64,
}

impl AgentStatus {
    /// The top record of `status`. `done` and `error` records reported
    /// at or before `seen` (the user saw them) don't count; `idle` ones do,
    /// last.
    pub fn of(status: &ProgramStatus, process: Option<&str>, seen: u64) -> Option<AgentStatus> {
        let top = status.top(|r| !(matches!(r.state, StatusState::Done | StatusState::Error) && r.seq <= seen))?;
        Some(Self::from_record(top, status, process))
    }

    fn from_record(r: &StatusRecord, status: &ProgramStatus, process: Option<&str>) -> AgentStatus {
        let who = match &r.app {
            Some(app) => Some(Agent::from_app(app).map_or_else(|| Identity::Other(app.clone()), Identity::Known)),
            None => Identity::of(status, process),
        };
        AgentStatus {
            who,
            id: r.id.clone(),
            state: r.state,
            kind: r.kind,
            progress: r.progress,
            title: r.title.clone(),
            msg: r.msg.clone(),
            seq: r.seq,
        }
    }

    /// The agent's name ("Claude Code"), or "A program".
    pub fn who(&self) -> &str {
        self.who.as_ref().map_or("A program", Identity::name)
    }

    /// What it's doing, in a few words.
    pub fn summary(&self) -> String {
        match self.state {
            StatusState::Blocked => match self.kind {
                Some(BlockedKind::Permission) => "Waiting for permission".into(),
                Some(BlockedKind::Question) => "Has a question".into(),
                Some(BlockedKind::Auth) => "Waiting for a sign-in".into(),
                None => "Waiting for you".into(),
            },
            StatusState::Working => match self.progress {
                Some(p) => format!("Working · {p} %"),
                None => "Working".into(),
            },
            StatusState::Done => "Done".into(),
            StatusState::Error => "Failed".into(),
            StatusState::Idle => "Idle".into(),
        }
    }

    /// The icon for its state, if it has one.
    pub fn icon(&self) -> Option<Icon> {
        Some(match self.state {
            StatusState::Blocked => match self.kind {
                Some(BlockedKind::Permission) => Icon::Shield,
                Some(BlockedKind::Question) => Icon::MessageCircleQuestion,
                Some(BlockedKind::Auth) => Icon::KeyRound,
                None => Icon::Hand,
            },
            StatusState::Done => Icon::CircleCheck,
            StatusState::Error => Icon::CircleX,
            StatusState::Working => Icon::Loader,
            StatusState::Idle => return None,
        })
    }

    /// The program's own words: its title and message, shortened.
    pub fn details(&self, max: usize) -> Option<String> {
        let text = match (&self.title, &self.msg) {
            (Some(t), Some(m)) => format!("{t}: {m}"),
            (Some(t), None) => t.clone(),
            (None, Some(m)) => m.clone(),
            (None, None) => return None,
        };
        Some(shorten(&text, max))
    }

    /// "Claude Code · Waiting for permission", then its own words.
    pub fn tooltip(&self) -> String {
        let head = format!("{} · {}", self.who(), self.summary());
        match self.details(280) {
            Some(d) => format!("{head}\n{d}"),
            None => head,
        }
    }

    /// The parts of it a change is judged by (see [`is_news`]).
    pub fn key(&self) -> StatusKey {
        StatusKey { id: self.id.clone(), state: self.state, kind: self.kind }
    }
}

/// Which record, in which state: what a notification was about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusKey {
    pub id: String,
    pub state: StatusState,
    pub kind: Option<BlockedKind>,
}

/// The status a pane's notifications follow: the top record, without idle
/// ones (an agent going idle after it finished isn't a change).
pub fn notable(status: &ProgramStatus, process: Option<&str>) -> Option<AgentStatus> {
    let top = status.top(|r| r.state != StatusState::Idle)?;
    Some(AgentStatus::from_record(top, status, process))
}

/// Is `now` worth a notification after `before`? Only a move into
/// blocked, done or error is, and only when it's not the same thing again
/// (a program repeating its report, a snapshot replaying it).
pub fn is_news(before: Option<&StatusKey>, now: &AgentStatus) -> bool {
    matches!(now.state, StatusState::Blocked | StatusState::Done | StatusState::Error) && before != Some(&now.key())
}

/// The most important of several panes' statuses (a split tab).
pub fn most_important<'a>(all: impl Iterator<Item = &'a AgentStatus>) -> Option<&'a AgentStatus> {
    all.filter(|s| s.state != StatusState::Idle).max_by_key(|s| (s.state.priority(), s.seq))
}

/// `text` on one line, at most `max` characters (an ellipsis when cut).
pub fn shorten(text: &str, max: usize) -> String {
    let one_line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max {
        return one_line;
    }
    let cut: String = one_line.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod tests {
    use goatty_term::StatusRecord;

    use super::*;

    fn record(id: &str, app: Option<&str>, state: StatusState, seq: u64) -> StatusRecord {
        StatusRecord { id: id.into(), app: app.map(Into::into), state, kind: None, progress: None, title: None, msg: None, seq }
    }

    fn status(records: Vec<StatusRecord>) -> ProgramStatus {
        let latest = records.iter().map(|r| r.seq).max().unwrap_or(0);
        ProgramStatus { records, progress: None, latest }
    }

    #[test]
    fn knows_agents_by_app_and_by_program() {
        assert_eq!(Agent::from_app("claude-code"), Some(Agent::ClaudeCode));
        assert_eq!(Agent::from_app("Codex"), Some(Agent::Codex));
        assert_eq!(Agent::from_app("agy"), Some(Agent::Antigravity));
        assert_eq!(Agent::from_app("vim"), None);
        assert_eq!(Agent::from_process("cursor-agent"), Some(Agent::Cursor));
        assert_eq!(Agent::from_process("zsh"), None);
        for a in Agent::ALL {
            if let Some(path) = a.logo().path() {
                assert!(logo_bytes(path).is_some(), "{a:?}");
            }
        }
        assert_eq!(LOGOS.len(), Agent::ALL.iter().filter(|a| a.logo().path().is_some()).count(), "every logo is used");
    }

    #[test]
    fn identity_prefers_the_report_and_lets_go_after_exit() {
        let working = status(vec![record("", Some("claude-code"), StatusState::Working, 1)]);
        assert_eq!(Identity::of(&working, Some("ssh")), Some(Identity::Known(Agent::ClaudeCode)));
        assert_eq!(Identity::of(&working, Some("node")), Some(Identity::Known(Agent::ClaudeCode)));
        let other = status(vec![record("", Some("my-bot"), StatusState::Idle, 1)]);
        assert_eq!(Identity::of(&other, None), Some(Identity::Other("my-bot".into())));
        // Done, and the shell is back: the shell's icon again.
        let done = status(vec![record("", Some("codex"), StatusState::Done, 2)]);
        assert_eq!(Identity::of(&done, Some("zsh")), None);
        assert_eq!(Identity::of(&done, Some("codex")), Some(Identity::Known(Agent::Codex)));
        assert_eq!(Identity::of(&ProgramStatus::default(), Some("gemini")), Some(Identity::Known(Agent::Gemini)));
    }

    #[test]
    fn status_hides_what_the_user_saw() {
        let st = status(vec![record("", Some("codex"), StatusState::Idle, 1), record("sub", None, StatusState::Done, 2)]);
        let top = AgentStatus::of(&st, None, 0).unwrap();
        assert_eq!(top.state, StatusState::Done);
        assert_eq!(top.who(), "Codex", "inherits the pane's agent");
        assert_eq!(AgentStatus::of(&st, None, 2).unwrap().state, StatusState::Idle);
        let blocked =
            status(vec![StatusRecord { kind: Some(BlockedKind::Question), ..record("", Some("opencode"), StatusState::Blocked, 3) }]);
        let s = AgentStatus::of(&blocked, None, 9).unwrap();
        assert_eq!(s.tooltip(), "OpenCode · Has a question");
        assert_eq!(s.icon(), Some(Icon::MessageCircleQuestion));
    }

    #[test]
    fn news_is_a_change_into_something_to_tell() {
        let rec = |state| AgentStatus { who: None, id: String::new(), state, kind: None, progress: None, title: None, msg: None, seq: 1 };
        let blocked = rec(StatusState::Blocked);
        assert!(is_news(None, &blocked));
        assert!(!is_news(Some(&blocked.key()), &blocked), "the same report again");
        assert!(is_news(Some(&rec(StatusState::Working).key()), &rec(StatusState::Done)));
        assert!(!is_news(None, &rec(StatusState::Working)));
        let permission = AgentStatus { kind: Some(BlockedKind::Permission), ..rec(StatusState::Blocked) };
        assert!(is_news(Some(&blocked.key()), &permission), "waiting for something else");
        let sub = AgentStatus { id: "sub".into(), ..rec(StatusState::Done) };
        assert!(is_news(Some(&rec(StatusState::Done).key()), &sub), "another record finished");
    }

    #[test]
    fn shortens_to_one_line() {
        assert_eq!(shorten("a\n  b", 10), "a b");
        assert_eq!(shorten("abcdefghij", 5), "abcd…");
        assert_eq!(shorten("abcde", 5), "abcde");
    }
}

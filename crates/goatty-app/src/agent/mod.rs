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
//! Colored logos are rasterized at exactly the size they're drawn in
//! device pixels (and cached): GPUI's `img()` would rasterize an SVG once
//! at a fixed size and shrink it on the GPU without mipmaps, which leaves
//! stray pixels at small sizes. Pixel-art marks are drawn at a whole
//! number of device pixels per cell.
//!
//! [`AgentStatus`] is what tabs show: the record that deserves the most
//! attention (blocked, error, working, done), with `done` and `error`
//! hidden once the user saw them. `notify` turns changes into
//! notifications.

pub mod notify;

use std::collections::HashMap;
use std::sync::Arc;

use goatty_term::{BlockedKind, ProgramStatus, StatusRecord, StatusState};
use gpui::{
    AnyElement, App, DevicePixels, Global, Hsla, ImageCacheError, IntoElement, ParentElement, RenderImage, Styled, SvgSize, Window, div,
    img, px, svg,
};

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
}

/// How an agent's logo is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Logo {
    /// One of the logo files.
    Mark {
        path: &'static str,
        paint: Paint,
        /// Pixel art `n` cells across its square viewBox: drawn at a whole
        /// number of logical pixels per cell, so its edges land on the
        /// pixel grid (the largest such size that fits, or one pixel per
        /// cell, overflowing the box a little).
        cells: Option<u16>,
    },
    /// No logo of its own: one of goatty's icons.
    Glyph(Icon),
}

/// What colors a logo is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    /// Its own (an image).
    Own,
    /// A black mark, tinted with the text color (so it shows on dark themes).
    Tint,
}

const fn mark(path: &'static str, paint: Paint) -> Logo {
    Logo::Mark { path, paint, cells: None }
}

const fn pixel_art(path: &'static str, paint: Paint, cells: u16) -> Logo {
    Logo::Mark { path, paint, cells: Some(cells) }
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
        }
    }

    pub fn logo(self) -> Logo {
        match self {
            Agent::ClaudeCode => pixel_art("agents/claude-code.svg", Paint::Own, 16),
            Agent::Codex => mark("agents/codex.svg", Paint::Own),
            Agent::OpenCode => pixel_art("agents/opencode.svg", Paint::Tint, 12),
            Agent::Antigravity => mark("agents/antigravity.svg", Paint::Own),
            Agent::Gemini => mark("agents/gemini-cli.svg", Paint::Own),
            Agent::Copilot => mark("agents/copilot.svg", Paint::Own),
            Agent::Cursor => mark("agents/cursor.svg", Paint::Tint),
            Agent::Goose => mark("agents/goose.svg", Paint::Tint),
            Agent::Pi => pixel_art("agents/pi.svg", Paint::Tint, 4),
            Agent::Amp => mark("agents/amp.svg", Paint::Own),
            Agent::Qwen => mark("agents/qwen.svg", Paint::Own),
        }
    }
}

impl Logo {
    /// The asset it's drawn from, unless it's one of goatty's icons.
    pub fn path(self) -> Option<&'static str> {
        match self {
            Logo::Mark { path, .. } => Some(path),
            Logo::Glyph(_) => None,
        }
    }

    /// The logo in a box `size` px square; `tint` colors the marks that
    /// take one.
    pub fn element(self, size: f32, tint: Hsla) -> AnyElement {
        let (path, paint, cells) = match self {
            Logo::Mark { path, paint, cells } => (path, paint, cells),
            Logo::Glyph(icon) => return icon.svg().size(px(size)).text_color(tint).into_any_element(),
        };
        let drawn = cells.map_or(size, |cells| pixel_size(size, cells));
        // `svg()` rasterizes at the exact size itself; images need help.
        let logo = match paint {
            Paint::Own => img(rasterized(path, drawn)).flex_none().size(px(drawn)).into_any_element(),
            Paint::Tint => svg().path(path).flex_none().size(px(drawn)).text_color(tint).into_any_element(),
        };
        if drawn == size {
            return logo;
        }
        // Centered on the box; a bigger drawing overflows it evenly.
        div().flex_none().size(px(size)).flex().items_center().justify_center().child(logo).into_any_element()
    }
}

/// The largest size up to `size` at which pixel art `cells` cells across
/// has a whole number of logical pixels per cell; one pixel per cell if
/// `size` is smaller than that.
fn pixel_size(size: f32, cells: u16) -> f32 {
    let cells = f32::from(cells);
    (size / cells).floor().max(1.0) * cells
}

/// Logos rasterized for the screen, by path and size in device pixels.
#[derive(Default)]
struct Rasterized(HashMap<(&'static str, u32), Arc<RenderImage>>);

impl Global for Rasterized {}

/// An `img()` source drawing logo `path` at exactly `size` logical pixels
/// in the window's device pixels.
fn rasterized(path: &'static str, size: f32) -> impl Fn(&mut Window, &mut App) -> Option<Result<Arc<RenderImage>, ImageCacheError>> {
    move |window, cx| {
        let device = (size * window.scale_factor()).round().max(1.0) as u32;
        if let Some(image) = cx.default_global::<Rasterized>().0.get(&(path, device)) {
            return Some(Ok(image.clone()));
        }
        let renderer = cx.svg_renderer();
        let side = DevicePixels(device as i32);
        let image =
            renderer.parse_svg(logo_bytes(path)?).and_then(|svg| renderer.render_parsed(&svg, SvgSize::ExactSize(gpui::size(side, side))));
        Some(match image {
            Ok(image) => {
                cx.default_global::<Rasterized>().0.insert((path, device), image.clone());
                Ok(image)
            }
            Err(e) => Err(e.into()),
        })
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

    /// `r`, one of `status`'s records (or one that was): who it's from is
    /// its `app`, else the pane's agent.
    pub fn from_record(r: &StatusRecord, status: &ProgramStatus, process: Option<&str>) -> AgentStatus {
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

    /// Which record, in which state (what a toast is about).
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

/// The status a pane's notifications are about: the top record, without
/// idle ones (an agent going idle after it finished hasn't moved on).
/// Which reports are news is `goatty_term::status`'s call.
pub fn notable(status: &ProgramStatus, process: Option<&str>) -> Option<AgentStatus> {
    let top = status.top(|r| r.state != StatusState::Idle)?;
    Some(AgentStatus::from_record(top, status, process))
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
    fn pixel_art_gets_whole_pixels_per_cell() {
        assert_eq!(pixel_size(14.0, 16), 16.0, "one pixel per cell, overflowing a little");
        assert_eq!(pixel_size(17.0, 16), 16.0);
        assert_eq!(pixel_size(32.0, 16), 32.0);
        assert_eq!(pixel_size(14.0, 4), 12.0);
        assert_eq!(pixel_size(17.0, 4), 16.0);
        assert_eq!(pixel_size(14.0, 12), 12.0);
    }

    #[test]
    fn shortens_to_one_line() {
        assert_eq!(shorten("a\n  b", 10), "a b");
        assert_eq!(shorten("abcdefghij", 5), "abcd…");
        assert_eq!(shorten("abcde", 5), "abcde");
    }
}

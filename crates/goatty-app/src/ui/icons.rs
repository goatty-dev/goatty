//! Icons: small stroke SVGs embedded in the binary and served to GPUI's
//! `svg()` element through [`Assets`].
//!
//! GPUI draws an SVG as a mask in the element's text color, so the files'
//! own colors don't matter. They are drawn on a 24×24 grid with 2px round
//! strokes (Lucide's style; most are Lucide icons, ISC licensed); add new
//! ones in the same style to `assets/icons/` and here.

use std::borrow::Cow;

use gpui::{AssetSource, SharedString, Styled, Svg, svg};

macro_rules! icons {
    ($($variant:ident => $file:literal),* $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Icon { $($variant),* }

        impl Icon {
            pub const ALL: &[Icon] = &[$(Icon::$variant),*];

            /// The asset path `svg().path(…)` takes.
            pub fn path(self) -> &'static str {
                match self { $(Icon::$variant => concat!("icons/", $file)),* }
            }

            fn bytes(self) -> &'static [u8] {
                match self { $(Icon::$variant => include_bytes!(concat!("../../assets/icons/", $file))),* }
            }
        }
    };
}

icons! {
    Activity => "activity.svg",
    CircleCheck => "circle-check.svg",
    CircleX => "circle-x.svg",
    Info => "info.svg",
    TriangleAlert => "triangle-alert.svg",
    Loader => "loader.svg",
    Close => "x.svg",
    Plus => "plus.svg",
    Terminal => "terminal.svg",
    AppWindow => "app-window.svg",
    ArrowLeft => "arrow-left.svg",
    ArrowRight => "arrow-right.svg",
    ArrowUp => "arrow-up.svg",
    ArrowDown => "arrow-down.svg",
    ArrowLeftRight => "arrow-left-right.svg",
    ArrowUpDown => "arrow-up-down.svg",
    Bell => "bell.svg",
    BookOpen => "book-open.svg",
    BookmarkPlus => "bookmark-plus.svg",
    Bot => "bot.svg",
    Bug => "bug.svg",
    CaseSensitive => "case-sensitive.svg",
    Check => "check.svg",
    ChevronDown => "chevron-down.svg",
    ChevronRight => "chevron-right.svg",
    ChevronUp => "chevron-up.svg",
    ChevronsUpDown => "chevrons-up-down.svg",
    ClipboardPaste => "clipboard-paste.svg",
    Code => "code.svg",
    Columns2 => "columns-2.svg",
    CommandKey => "command.svg",
    Copy => "copy.svg",
    Download => "download.svg",
    Eraser => "eraser.svg",
    ExternalLink => "external-link.svg",
    FileText => "file-text.svg",
    FolderOpen => "folder-open.svg",
    GitCommit => "git-commit.svg",
    Globe => "globe.svg",
    Hand => "hand.svg",
    History => "history.svg",
    KeyRound => "key-round.svg",
    Keyboard => "keyboard.svg",
    Link => "link.svg",
    Link2 => "link-2.svg",
    ListFilter => "list-filter.svg",
    Maximize2 => "maximize-2.svg",
    Menu => "menu.svg",
    MessageCircleQuestion => "message-circle-question.svg",
    Monitor => "monitor.svg",
    MousePointerClick => "mouse-pointer-click.svg",
    Package => "package.svg",
    PanelLeftClose => "panel-left-close.svg",
    PanelLeftOpen => "panel-left-open.svg",
    Palette => "palette.svg",
    Pencil => "pencil.svg",
    Puzzle => "puzzle.svg",
    Refresh => "refresh.svg",
    Regex => "regex.svg",
    RotateCcw => "rotate-ccw.svg",
    RotateCw => "rotate-cw.svg",
    Rows2 => "rows-2.svg",
    Search => "search.svg",
    Server => "server.svg",
    Shell => "shell.svg",
    Shield => "shield.svg",
    ShieldCheck => "shield-check.svg",
    Sliders => "sliders.svg",
    SquareArrowOutUpRight => "square-arrow-out-up-right.svg",
    SquareTerminal => "square-terminal.svg",
    TextCursorInput => "text-cursor-input.svg",
    Trash => "trash.svg",
    Unlink => "unlink.svg",
    Upload => "upload.svg",
    UserRound => "user-round.svg",
    WrapText => "wrap-text.svg",
    Zap => "zap.svg",
}

impl Icon {
    /// An `svg()` element showing this icon (size and color are the caller's).
    pub fn svg(self) -> Svg {
        svg().path(self.path()).flex_none()
    }

    /// The icon by its file name without `.svg` (`"file-text"`): how
    /// plugins name theirs (`goatty_plugin::commands::MENU_ICONS`).
    pub fn by_name(name: &str) -> Option<Icon> {
        Icon::ALL.iter().copied().find(|i| i.path().strip_prefix("icons/").and_then(|p| p.strip_suffix(".svg")) == Some(name))
    }
}

/// goatty's embedded assets, installed with `Application::with_assets`:
/// the icons and the coding agents' logos (`crate::agent`).
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> anyhow::Result<Option<Cow<'static, [u8]>>> {
        let icon = Icon::ALL.iter().find(|i| i.path() == path).map(|i| i.bytes());
        Ok(icon.or_else(|| crate::agent::logo_bytes(path)).map(Cow::Borrowed))
    }

    fn list(&self, path: &str) -> anyhow::Result<Vec<SharedString>> {
        let all = Icon::ALL.iter().map(|i| i.path()).chain(crate::agent::Agent::ALL.iter().filter_map(|a| a.logo().path()));
        Ok(all.filter(|p| p.starts_with(path)).map(SharedString::from).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_loads_and_is_svg() {
        for icon in Icon::ALL {
            let bytes = Assets.load(icon.path()).unwrap().unwrap_or_else(|| panic!("{icon:?} missing"));
            assert!(std::str::from_utf8(&bytes).unwrap().starts_with("<svg"), "{icon:?}");
        }
        assert!(Assets.load("icons/nope.svg").unwrap().is_none());
        assert_eq!(Assets.list("icons/").unwrap().len(), Icon::ALL.len());
        for path in Assets.list("agents/").unwrap() {
            let bytes = Assets.load(&path).unwrap().unwrap();
            assert!(std::str::from_utf8(&bytes).unwrap().starts_with("<svg"), "{path}");
        }
    }

    #[test]
    fn every_plugin_menu_icon_exists() {
        for name in goatty_plugin::commands::MENU_ICONS {
            assert!(Icon::by_name(name).is_some(), "{name}");
        }
        assert_eq!(Icon::by_name("file-text"), Some(Icon::FileText));
        assert_eq!(Icon::by_name("nope"), None);
    }

    #[test]
    fn every_icon_file_is_listed() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/icons");
        for f in std::fs::read_dir(dir).unwrap() {
            let name = f.unwrap().file_name().to_string_lossy().into_owned();
            assert!(Icon::ALL.iter().any(|i| i.path() == format!("icons/{name}")), "{name} isn't in icons.rs");
        }
    }
}

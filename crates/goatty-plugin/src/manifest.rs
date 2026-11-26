//! `plugin.toml`: what a plugin is, what it installs, which settings it
//! exposes and how they reach the tool. See `docs/plugins.md` for the
//! format with examples.
//!
//! A manifest is data, never code goatty runs to find out what a plugin
//! does: everything security-relevant (downloads, programs on `PATH`,
//! aliases, environment variables, shell code) is visible here, so goatty
//! can show it before installing and derive the plugin's permissions from
//! it ([`crate::permissions`]).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The manifest format this goatty reads. A newer `schema` is refused with
/// "needs a newer goatty" instead of being half-understood.
pub const SCHEMA: u32 = 1;

/// The file name of a manifest inside a plugin directory.
pub const FILE: &str = "plugin.toml";

/// Download targets goatty knows (`"<arch>-<os>"`).
pub const TARGETS: &[&str] = &["x86_64-linux", "aarch64-linux", "x86_64-macos", "aarch64-macos", "x86_64-windows", "aarch64-windows"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Manifest format version ([`SCHEMA`]).
    pub schema: u32,
    /// Stable identifier: lowercase letters, digits and `-`, 2–40 characters.
    pub id: String,
    /// Display name.
    pub name: String,
    /// The plugin's own version (semver). Independent of the tool version:
    /// bump it whenever the manifest or its files change.
    pub version: String,
    /// One line for lists.
    pub summary: String,
    /// A few sentences for the details view: what the tool is and why you'd want it.
    #[serde(default)]
    pub description: String,
    /// Who maintains this plugin (not necessarily the tool's author).
    pub publisher: String,
    #[serde(default)]
    pub homepage: Option<String>,
    /// The tool's license (SPDX).
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub category: Category,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub compat: Compat,
    #[serde(default)]
    pub requires: Requires,
    #[serde(default, rename = "package")]
    pub packages: Vec<Package>,
    #[serde(default, rename = "setting")]
    pub settings: Vec<Setting>,
    #[serde(default, rename = "alias")]
    pub aliases: Vec<Alias>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default, rename = "file")]
    pub files: Vec<ConfigFile>,
    #[serde(default)]
    pub zsh: ZshIntegration,
    /// PowerShell integration for Windows managed PowerShell (#278).
    #[serde(default, skip_serializing_if = "PowerShellIntegration::is_empty")]
    pub powershell: PowerShellIntegration,
    /// Entries in goatty's command palette (see [`crate::commands`]).
    #[serde(default, rename = "command", skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<crate::commands::PaletteCommand>,
    /// Command lines with blanks for the snippet picker (see [`crate::snippets`]).
    #[serde(default, rename = "snippet", skip_serializing_if = "Vec::is_empty")]
    pub snippets: Vec<crate::snippets::Snippet>,
    /// Words that turn into commands while you type (see [`crate::snippets`]).
    #[serde(default, rename = "expansion", skip_serializing_if = "Vec::is_empty")]
    pub expansions: Vec<crate::snippets::Expansion>,
    /// Why the plugin needs each permission it uses: `binaries = "..."`.
    /// See [`crate::permissions`].
    #[serde(default)]
    pub permissions: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Category {
    /// Command-line tools, aliases, environment, completions.
    #[default]
    Environment,
    /// goatty commands and UI features (needs a runtime goatty doesn't have yet).
    Application,
}

impl Category {
    pub fn label(self) -> &'static str {
        match self {
            Category::Environment => "environment",
            Category::Application => "application",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Compat {
    /// Semver requirement on the goatty version, e.g. `">=0.1"`. Empty: any.
    #[serde(default)]
    pub goatty: String,
    /// `linux`, `macos`, `windows`. Empty: wherever its packages have a build.
    #[serde(default)]
    pub os: Vec<String>,
    /// Shells it integrates with. Empty: `["zsh"]`.
    #[serde(default)]
    pub shells: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requires {
    /// Commands that must already be installed on the host (goatty doesn't
    /// install them), e.g. `docker`. The integration is skipped without them.
    #[serde(default)]
    pub commands: Vec<String>,
    /// Other plugins that must be installed first.
    #[serde(default)]
    pub plugins: Vec<String>,
}

/// A tool the plugin installs into goatty's data directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Package {
    /// Directory name under `tools/` (lowercase, digits, `-`).
    pub id: String,
    /// The tool's version (as upstream names it).
    pub version: String,
    /// Executables to put on the shell's `PATH`, relative to the asset's
    /// `root`. The link is named after the file (`bin/eza`). May be empty
    /// for a package that only has a [`source`](Self::source) script.
    #[serde(default)]
    pub bin: Vec<String>,
    /// More names for the package's one executable, for a multi-call
    /// program that acts on the name it's started as (busybox, Microsoft's
    /// coreutils): each is linked to it in `bin/` (`bin/ls` → `coreutils`;
    /// a hard link `ls.exe` on Windows), so other programs (`xargs`,
    /// `find -exec`) find them on `PATH` too, not only the shell.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub applets: Vec<String>,
    /// A shell script in the package (relative to the asset's `root`) that
    /// goatty loads at the plugin's place in the load order, after its
    /// shell code ran: a `.zsh` file for zsh (a zsh plugin like
    /// zsh-autosuggestions, sourced), a `.ps1` (dot-sourced) or `.psm1`
    /// (imported as a module) for PowerShell. The package only applies to
    /// that shell. The package directory is linked as `plugins/<id>/`.
    #[serde(default)]
    pub source: Option<String>,
    /// Where distributions put that script (absolute paths under `/usr/`,
    /// `/opt/homebrew/`, `/opt/local/` or a Nix profile), checked in order
    /// before goatty's own copy when `prefer_system` is on.
    #[serde(default)]
    pub system_paths: Vec<String>,
    /// Use a copy that's already on `PATH` instead of downloading one.
    #[serde(default = "yes")]
    pub prefer_system: bool,
    /// A command that prints the tool's completions ([`Completions`]), like
    /// `["starship", "completions", "zsh"]`, or one per shell. goatty runs
    /// it after installing its own copy and keeps the output (zsh: `_<name>`
    /// on the managed shell's `fpath`; PowerShell: a script dot-sourced at
    /// startup). Copies on the system come with their own completions, so
    /// it doesn't run for them.
    #[serde(default, skip_serializing_if = "Completions::is_empty")]
    pub completions: Completions,
    #[serde(rename = "asset")]
    pub assets: Vec<Asset>,
}

fn yes() -> bool {
    true
}

/// How a package prints its completions (`[[package]] completions`).
///
/// * A list: one command for every shell, written for zsh
///   (`["fd", "--gen-completions", "zsh"]`). For PowerShell goatty swaps the
///   shell's name in the arguments: `zsh` → `powershell`, `complete-zsh` →
///   `complete-powershell`, `--shell=zsh` → `--shell=powershell`.
/// * A table, for tools that name shells their own way or need other
///   arguments: `{ zsh = ["bat", "--completion", "zsh"], powershell = ["bat",
///   "--completion", "ps1"] }`. A shell that isn't listed gets none.
///
/// The first word is one of the package's executables (its name, as in
/// `bin/`), the rest its arguments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Completions {
    Command(Vec<String>),
    PerShell(BTreeMap<String, Vec<String>>),
}

impl Default for Completions {
    fn default() -> Self {
        Completions::Command(Vec::new())
    }
}

impl From<Vec<String>> for Completions {
    fn from(v: Vec<String>) -> Self {
        Completions::Command(v)
    }
}

impl Completions {
    pub fn is_empty(&self) -> bool {
        match self {
            Completions::Command(v) => v.is_empty(),
            Completions::PerShell(m) => m.values().all(Vec::is_empty),
        }
    }

    /// Every command it can run (for validation and the install plan).
    pub fn commands(&self) -> Vec<(&str, &[String])> {
        match self {
            Completions::Command(v) if v.is_empty() => Vec::new(),
            Completions::Command(v) => vec![("", v.as_slice())],
            Completions::PerShell(m) => m.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (k.as_str(), v.as_slice())).collect(),
        }
    }

    /// The command for `shell` (`"zsh"`, `"powershell"`); `None`: no completions there.
    pub fn for_shell(&self, shell: &str) -> Option<Vec<String>> {
        match self {
            Completions::Command(v) if v.is_empty() => None,
            Completions::Command(v) if shell == "zsh" => Some(v.clone()),
            Completions::Command(v) => Some(v.iter().map(|a| swap_shell_name(a, shell)).collect()),
            Completions::PerShell(m) => m.get(shell).filter(|v| !v.is_empty()).cloned(),
        }
    }

    /// As the install plan shows it: `fd --gen-completions zsh`, or one per shell.
    pub fn describe(&self) -> String {
        match self {
            Completions::Command(v) => v.join(" "),
            Completions::PerShell(m) => {
                m.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| format!("{k}: {}", v.join(" "))).collect::<Vec<_>>().join("; ")
            }
        }
    }
}

/// `zsh`, `complete-zsh`, `--shell=zsh` with `shell` in place of `zsh`.
fn swap_shell_name(arg: &str, shell: &str) -> String {
    if arg == "zsh" {
        return shell.to_owned();
    }
    for sep in ['-', '='] {
        if let Some(head) = arg.strip_suffix("zsh").filter(|h| h.ends_with(sep)) {
            return format!("{head}{shell}");
        }
    }
    arg.to_owned()
}

/// One downloadable build of a package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    /// `"<arch>-<os>"` (see [`TARGETS`]) or `"any"`.
    pub target: String,
    /// `https://` only.
    pub url: String,
    /// Lowercase hex sha256 of the download. Nothing is unpacked or run
    /// before it matches.
    pub sha256: String,
    /// Download size in bytes (shown before installing).
    pub size: u64,
    #[serde(default)]
    pub format: AssetFormat,
    /// Directory inside the archive holding the files (`""`: the top).
    #[serde(default)]
    pub root: String,
}

/// What a download is: an archive to unpack (`crate::archive`), or the
/// executable itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AssetFormat {
    #[default]
    #[serde(rename = "tar.gz")]
    TarGz,
    #[serde(rename = "tar.xz")]
    TarXz,
    #[serde(rename = "tar.zst")]
    TarZst,
    #[serde(rename = "tar")]
    Tar,
    #[serde(rename = "zip")]
    Zip,
    /// The download is the executable itself (one `bin` entry).
    #[serde(rename = "binary")]
    Binary,
}

impl AssetFormat {
    /// Every format, as written in `plugin.toml`.
    pub const ALL: [AssetFormat; 6] = [Self::TarGz, Self::TarXz, Self::TarZst, Self::Tar, Self::Zip, Self::Binary];

    /// The name in `plugin.toml` (`"tar.gz"`).
    pub fn name(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::TarXz => "tar.xz",
            Self::TarZst => "tar.zst",
            Self::Tar => "tar",
            Self::Zip => "zip",
            Self::Binary => "binary",
        }
    }

    /// The archive format a URL's file name ends in (`.zip`, `.tgz`, …),
    /// ignoring a query or fragment; `None` for anything else.
    pub fn from_url(url: &str) -> Option<Self> {
        let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
        [
            (".tar.gz", Self::TarGz),
            (".tgz", Self::TarGz),
            (".tar.xz", Self::TarXz),
            (".txz", Self::TarXz),
            (".tar.zst", Self::TarZst),
            (".tzst", Self::TarZst),
            (".tar", Self::Tar),
            (".zip", Self::Zip),
        ]
        .into_iter()
        .find_map(|(ext, f)| path.ends_with(ext).then_some(f))
    }
}

/// A setting shown in goatty's settings window, with an explanation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    /// Key in `host.json` (`plugins.<plugin>.settings.<id>`) and in
    /// templates (`{id}`): lowercase letters, digits, `_`.
    pub id: String,
    /// Short label.
    pub label: String,
    /// What it does, in plain words: shown under the label.
    #[serde(default)]
    pub help: String,
    #[serde(rename = "type")]
    pub kind: SettingKind,
    /// Value when the user hasn't set one. Required, except for `string`,
    /// `path` (empty) and `list` (no entries).
    #[serde(default)]
    pub default: Option<Value>,
    /// Heading the setting is grouped under.
    #[serde(default)]
    pub group: Option<String>,
    /// Hidden behind "Show advanced settings".
    #[serde(default)]
    pub advanced: bool,
    /// `choice`: the options.
    #[serde(default)]
    pub choices: Vec<Choice>,
    /// `integer`: bounds and step.
    #[serde(default)]
    pub min: Option<i64>,
    #[serde(default)]
    pub max: Option<i64>,
    #[serde(default)]
    pub step: Option<i64>,
    /// `integer`: unit shown after the number (`ms`, `lines`).
    #[serde(default)]
    pub unit: Option<String>,
    /// `string`, `path`, `list`: hint shown in an empty field.
    #[serde(default)]
    pub placeholder: Option<String>,
    /// `list`: how `{id}` joins the entries (default `" "`). `"path"`: the
    /// host's path-list separator (`:`, or `;` on Windows, where `:` is part
    /// of `C:\`), for variables like `_ZO_EXCLUDE_DIRS`.
    #[serde(default)]
    pub separator: Option<String>,
    /// The command-line argument this setting adds (`{id.arg}`, `{args}`).
    /// `bool`: added when on. Others: `{value}` is replaced by the value;
    /// nothing is added for an empty value. `list`: one argument per entry.
    #[serde(default)]
    pub arg: Option<String>,
    /// `bool`: the argument when off.
    #[serde(default)]
    pub arg_off: Option<String>,
    /// Only on these OSes ([`Platform`]); empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// Only with these managed shells; empty: every shell the plugin supports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shells: Vec<String>,
}

/// [`Setting::separator`] that means the host's path-list separator.
pub const PATH_SEPARATOR: &str = "path";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingKind {
    Bool,
    Choice,
    String,
    Integer,
    Path,
    List,
}

impl SettingKind {
    pub fn label(self) -> &'static str {
        match self {
            SettingKind::Bool => "bool",
            SettingKind::Choice => "choice",
            SettingKind::String => "string",
            SettingKind::Integer => "integer",
            SettingKind::Path => "path",
            SettingKind::List => "list",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Choice {
    pub value: String,
    pub label: String,
    #[serde(default)]
    pub help: String,
    /// Overrides the setting's `arg` for this choice (`""`: no argument).
    #[serde(default)]
    pub arg: Option<String>,
}

/// `alias name='command args…'`, defined only when `command` is installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Alias {
    pub name: String,
    /// One of the plugin's package executables or required commands.
    pub command: String,
    /// Templates (see [`crate::template`]).
    #[serde(default)]
    pub args: Vec<String>,
    /// Condition on settings (see [`Condition`]).
    #[serde(default)]
    pub when: Option<String>,
    /// Shown in the install plan and the settings window.
    #[serde(default)]
    pub description: String,
    /// Only on these OSes; empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// Only with these managed shells; empty: every shell the plugin supports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shells: Vec<String>,
}

/// An exported environment variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvVar {
    pub name: String,
    /// A template; an empty result unsets nothing and exports nothing.
    pub value: String,
    #[serde(default)]
    pub when: Option<String>,
    /// Only on these OSes; empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// Only with these managed shells; empty: every shell the plugin supports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shells: Vec<String>,
}

/// A config file goatty generates from the settings (into the shell
/// configuration generation), optionally pointed at by an env variable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// File name (letters, digits, `.`, `_`, `-`).
    pub name: String,
    #[serde(default)]
    pub format: FileFormat,
    /// `lines`: one template per line (whole-item `{id.arg}`/`{args}` allowed).
    #[serde(default)]
    pub lines: Vec<String>,
    /// `text`: the file, with `{id}` replaced by values.
    #[serde(default)]
    pub template: Option<String>,
    /// Export this variable with the file's path.
    #[serde(default)]
    pub env: Option<String>,
    #[serde(default)]
    pub when: Option<String>,
    /// Only on these OSes; empty: everywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub os: Vec<String>,
    /// Only with these managed shells; empty: every shell the plugin supports.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shells: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileFormat {
    #[default]
    Lines,
    Text,
}

/// Shell code shipped with the plugin. This is the one place a plugin runs
/// code of its own in every shell (`shell-code` permission).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ZshIntegration {
    /// A file in the plugin directory, sourced at top level after goatty's
    /// own features (before the user's additions and syntax highlighting).
    /// `$goatty_settings[<id>]` holds each setting's value while it runs.
    #[serde(default)]
    pub init: Option<String>,
    /// Directories in the plugin directory added to `fpath` before
    /// `compinit` (completion functions).
    #[serde(default)]
    pub fpath: Vec<String>,
    #[serde(default)]
    pub when: Option<String>,
    /// Where in the managed `.zshrc` the plugin loads ([`Load`]).
    #[serde(default)]
    pub load: Load,
    /// Plugins this one loads after when they're there (same `load` slot;
    /// an earlier slot always comes first). fzf-tab loads after fzf, so
    /// it wraps Tab after fzf's bindings did.
    #[serde(default)]
    pub after: Vec<String>,
}

/// PowerShell code shipped with the plugin (#278). Like [`ZshIntegration`],
/// `init` is the one place a plugin runs code of its own (`shell-code` permission).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PowerShellIntegration {
    /// A `.ps1` file in the plugin directory, dot-sourced at the plugin's
    /// load slot. `$goatty_settings` holds each setting's value while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
    /// Where in the managed PowerShell startup the plugin loads ([`Load`]).
    #[serde(default, skip_serializing_if = "is_default_load")]
    pub load: Load,
    /// Plugins this one loads after when they're there (same `load` slot).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
}

fn is_default_load(l: &Load) -> bool {
    *l == Load::Normal
}

impl PowerShellIntegration {
    pub fn is_empty(&self) -> bool {
        self.init.is_none() && self.when.is_none() && self.load == Load::Normal && self.after.is_empty()
    }
}

/// The slots of the managed `.zshrc` (and PowerShell `init.ps1`) a plugin can load in, in order. The
/// plugin's environment and `fpath` always go in before `compinit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Load {
    /// After completion is set up, before the prompt: tools that add line
    /// editor widgets and key bindings (fzf, fzf-tab, autosuggestions).
    LineEditor,
    /// The prompt. When no prompt plugin loads, goatty sets a small one.
    Prompt,
    /// After goatty's prompt, before the user's aliases and additions.
    #[default]
    Normal,
    /// After the user's additions (`~/.config/goatty/zshrc`): only for what
    /// has to wrap everything defined before it (syntax highlighting).
    Last,
}

impl Load {
    pub const ALL: [Load; 4] = [Load::LineEditor, Load::Prompt, Load::Normal, Load::Last];

    pub fn key(self) -> &'static str {
        match self {
            Load::LineEditor => "line-editor",
            Load::Prompt => "prompt",
            Load::Normal => "normal",
            Load::Last => "last",
        }
    }

    /// The file of the shell configuration generation this slot is
    /// rendered into (sourced by the managed `.zshrc` at its place).
    pub fn file(self) -> &'static str {
        match self {
            Load::LineEditor => "plugins-line-editor.zsh",
            Load::Prompt => "plugins-prompt.zsh",
            Load::Normal => "plugins.zsh",
            Load::Last => "plugins-last.zsh",
        }
    }

    /// The PowerShell file of the shell configuration generation this slot is
    /// rendered into (dot-sourced by the managed `init.ps1` at its place).
    pub fn ps_file(self) -> &'static str {
        match self {
            Load::LineEditor => "plugins-line-editor.ps1",
            Load::Prompt => "plugins-prompt.ps1",
            Load::Normal => "plugins.ps1",
            Load::Last => "plugins-last.ps1",
        }
    }
}

/// Where a plugin is used: a host's OS and its managed shell.
/// [`Manifest::for_platform`] narrows a manifest to one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Platform {
    /// `linux`, `macos`, `windows` (the OS part of a target).
    pub os: String,
    /// `zsh` or `powershell`.
    pub shell: &'static str,
}

impl Platform {
    /// The platform of download target `target` (`x86_64-windows`).
    pub fn of_target(target: &str) -> Platform {
        Platform { os: target.rsplit('-').next().unwrap_or("").to_owned(), shell: Manifest::managed_shell_for(target) }
    }

    /// This build's platform.
    pub fn host() -> Platform {
        Self::of_target(&crate::current_target())
    }

    /// What separates the entries of a path list (`PATH`) here.
    pub fn path_separator(&self) -> &'static str {
        if self.os == "windows" { ";" } else { ":" }
    }

    /// Whether an item limited to `os` and `shells` (empty: no limit) is used here.
    pub fn allows(&self, os: &[String], shells: &[String]) -> bool {
        (os.is_empty() || os.contains(&self.os)) && (shells.is_empty() || shells.iter().any(|s| s == self.shell))
    }
}

/// A target whose builds goatty also takes when a package has none of its
/// own: Windows on ARM runs x64 programs (emulated).
pub fn fallback_target(target: &str) -> Option<&'static str> {
    (target == "aarch64-windows").then_some("x86_64-windows")
}

impl Manifest {
    pub fn setting(&self, id: &str) -> Option<&Setting> {
        self.settings.iter().find(|s| s.id == id)
    }

    pub fn semver(&self) -> Option<semver::Version> {
        semver::Version::parse(&self.version).ok()
    }

    /// Every executable the plugin's packages put on `PATH` (their
    /// programs and applets).
    pub fn executables(&self) -> impl Iterator<Item = &str> {
        self.packages.iter().flat_map(|p| p.bin.iter().map(|b| bin_name(b)).chain(p.applets.iter().map(String::as_str)))
    }

    /// The packages' shell scripts goatty loads: (package, script).
    pub fn sources(&self) -> impl Iterator<Item = (&Package, &str)> {
        self.packages.iter().filter_map(|p| p.source.as_deref().map(|s| (p, s)))
    }

    /// The packages' scripts for `shell` (by their extension, [`script_shell`]).
    pub fn sources_for<'a>(&'a self, shell: &'a str) -> impl Iterator<Item = (&'a Package, &'a str)> {
        self.sources().filter(move |(_, src)| script_shell(src) == Some(shell))
    }

    /// The shells it integrates with (default zsh).
    pub fn shells(&self) -> Vec<&str> {
        if self.compat.shells.is_empty() { vec!["zsh"] } else { self.compat.shells.iter().map(String::as_str).collect() }
    }

    /// The managed shell goatty uses on the OS of `target`.
    pub fn managed_shell_for(target: &str) -> &'static str {
        match target.rsplit('-').next().unwrap_or("") {
            "windows" => "powershell",
            _ => "zsh",
        }
    }

    /// Whether this goatty version, OS and target can use the plugin; the
    /// reason when not.
    pub fn compatible(&self, goatty_version: &str, target: &str) -> Result<(), String> {
        if !self.compat.goatty.is_empty()
            && let (Ok(req), Ok(v)) = (semver::VersionReq::parse(&self.compat.goatty), semver::Version::parse(goatty_version))
            && !req.matches(&v)
        {
            return Err(format!("needs goatty {} (this is {goatty_version})", self.compat.goatty));
        }
        let os = target.rsplit('-').next().unwrap_or("");
        if !self.compat.os.is_empty() && !self.compat.os.iter().any(|o| o == os) {
            return Err(format!("supports {} only", self.compat.os.join(", ")));
        }
        let managed_shell = Self::managed_shell_for(target);
        if !self.shells().contains(&managed_shell) {
            return Err(format!("integrates with {} only; goatty's managed shell is {managed_shell}", self.shells().join(", ")));
        }
        for p in self.packages.iter().filter(|p| p.applies_to_shell(managed_shell)) {
            if p.asset_for(target).is_none() {
                return Err(format!("{} has no build for {target}", p.id));
            }
        }
        Ok(())
    }

    /// The manifest as a host on `p` uses it: settings, aliases, variables,
    /// files, palette commands and snippets limited to other OSes or shells
    /// (`os`, `shells`) are left out, and so is anything whose `when` tests
    /// a setting that was left out; packages of another shell are left out;
    /// a list's `separator = "path"` becomes the host's separator and
    /// `completions` the host shell's command.
    ///
    /// goattyd renders this, plans with it and hands it to clients, so a
    /// GUI shows the settings of the host it looks at whatever it runs on
    /// itself. The result has no `os`/`shells` limits and no per-shell
    /// completions left, which also keeps it readable for older clients.
    /// Stored values of the settings that were left out stay as they are
    /// (the shared shell config carries them to the hosts that use them).
    pub fn for_platform(&self, p: &Platform) -> Manifest {
        let mut m = self.clone();
        let dropped: BTreeSet<String> = m.settings.iter().filter(|s| !p.allows(&s.os, &s.shells)).map(|s| s.id.clone()).collect();
        m.settings.retain(|s| !dropped.contains(&s.id));
        for s in &mut m.settings {
            s.os.clear();
            s.shells.clear();
            if s.separator.as_deref() == Some(PATH_SEPARATOR) {
                s.separator = Some(p.path_separator().to_owned());
            }
        }
        let tests_dropped =
            |when: &Option<String>| when.as_deref().and_then(|w| Condition::parse(w).ok()).is_some_and(|c| dropped.contains(c.setting()));
        let keep = |os: &mut Vec<String>, shells: &mut Vec<String>, when: &Option<String>| {
            let kept = p.allows(os, shells) && !tests_dropped(when);
            os.clear();
            shells.clear();
            kept
        };
        m.aliases.retain_mut(|a| keep(&mut a.os, &mut a.shells, &a.when));
        m.env.retain_mut(|e| keep(&mut e.os, &mut e.shells, &e.when));
        m.files.retain_mut(|f| keep(&mut f.os, &mut f.shells, &f.when));
        m.commands.retain_mut(|c| keep(&mut c.os, &mut c.shells, &c.when));
        m.snippets.retain_mut(|s| keep(&mut s.os, &mut s.shells, &None));
        m.packages.retain(|pk| pk.applies_to_shell(p.shell));
        for pk in &mut m.packages {
            pk.completions = Completions::Command(pk.completions.for_shell(p.shell).unwrap_or_default());
        }
        m
    }
}

impl Package {
    /// The build for `target`: its own, else the [`fallback_target`]'s,
    /// else one for `any`.
    pub fn asset_for(&self, target: &str) -> Option<&Asset> {
        let find = |t: &str| self.assets.iter().find(|a| a.target == t);
        find(target).or_else(|| fallback_target(target).and_then(find)).or_else(|| find("any"))
    }

    /// Whether this package is used by `shell` (`"zsh"` or `"powershell"`).
    /// A package that only ships a [`source`](Self::source) script (no
    /// `bin`) is for the shell of that script: `.zsh` for zsh, `.ps1` or
    /// `.psm1` for PowerShell.
    pub fn applies_to_shell(&self, shell: &str) -> bool {
        match (&self.source, self.bin.is_empty()) {
            (Some(src), true) => script_shell(src) == Some(shell),
            _ => true,
        }
    }
}

/// The shell a package script is for, by its extension.
pub fn script_shell(path: &str) -> Option<&'static str> {
    if path.ends_with(".zsh") {
        Some("zsh")
    } else if path.ends_with(".ps1") || path.ends_with(".psm1") {
        Some("powershell")
    } else {
        None
    }
}

/// `bin/eza` → `eza`, `bin/tool.exe` → `tool.exe`.
pub fn bin_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Logical command name for a `Package.bin` entry: strips directory prefix and
/// any trailing `.exe` suffix (`bin/tool.exe` → `tool`, `starship` → `starship`).
pub fn command_name(path: &str) -> &str {
    let name = bin_name(path);
    if name.len() > 4 && name[name.len() - 4..].eq_ignore_ascii_case(".exe") { &name[..name.len() - 4] } else { name }
}

/// A problem in a manifest: where (`setting.icons.default`) and what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub path: String,
    pub message: String,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() { f.write_str(&self.message) } else { write!(f, "{}: {}", self.path, self.message) }
    }
}

/// Errors from [`parse`]: the manifest is unusable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invalid(pub Vec<Problem>);

impl std::fmt::Display for Invalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lines: Vec<String> = self.0.iter().map(ToString::to_string).collect();
        write!(f, "invalid plugin manifest:\n  {}", lines.join("\n  "))
    }
}

impl std::error::Error for Invalid {}

/// Parse and validate a manifest. `files`: the files of the plugin
/// directory (relative paths with `/`), to check that referenced files
/// exist; `None` skips that check.
pub fn parse(text: &str, files: Option<&BTreeSet<String>>) -> Result<Manifest, Invalid> {
    let one = |path: &str, message: String| Invalid(vec![Problem { path: path.into(), message }]);
    let raw: toml::Table = toml::from_str(text).map_err(|e| one("", format!("not valid TOML: {}", e.message())))?;
    match raw.get("schema").and_then(toml::Value::as_integer) {
        None => return Err(one("schema", format!("missing; this goatty reads schema = {SCHEMA}"))),
        Some(n) if n > i64::from(SCHEMA) => {
            return Err(one("schema", format!("schema {n} needs a newer goatty (this one reads schema {SCHEMA})")));
        }
        Some(n) if n < 1 => return Err(one("schema", format!("{n} is not a schema version"))),
        Some(_) => {}
    }
    let m: Manifest = toml::from_str(text).map_err(|e| one("", e.message().to_string()))?;
    let has_powershell_section = raw.contains_key("powershell");
    let problems = validate(&m, files, has_powershell_section);
    if problems.is_empty() { Ok(m) } else { Err(Invalid(problems)) }
}

/// Read `dir/plugin.toml` and check it against the directory's files.
pub fn load_dir(dir: &Path) -> anyhow::Result<Manifest> {
    let text = std::fs::read_to_string(dir.join(FILE)).map_err(|e| anyhow::anyhow!("read {}: {e}", dir.join(FILE).display()))?;
    let files = crate::bundle::list_files(dir)?;
    Ok(parse(&text, Some(&files.keys().cloned().collect()))?)
}

pub fn is_id(s: &str) -> bool {
    (2..=40).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && s.as_bytes()[0].is_ascii_lowercase()
        && !s.ends_with('-')
}

fn is_setting_id(s: &str) -> bool {
    (1..=40).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        && s.as_bytes()[0].is_ascii_lowercase()
        // Reserved by the templates.
        && s != "args"
        && s != "value"
}

/// A word that's safe as a file or directory name and in a version string.
fn is_plain(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
        && s != "."
        && s != ".."
        && !s.starts_with('.')
}

/// Alias names: what zsh takes without quoting surprises.
fn is_alias_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))
        && !s.starts_with('-')
        && !s.starts_with('.')
}

pub fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        && !s.as_bytes()[0].is_ascii_digit()
}

/// Variables a plugin may not set: they'd change how every program or the
/// shell itself behaves, or goatty's own startup.
const PROTECTED_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "SHELL",
    "ZDOTDIR",
    "FPATH",
    "LD_PRELOAD",
    "LD_LIBRARY_PATH",
    "DYLD_INSERT_LIBRARIES",
    "DYLD_LIBRARY_PATH",
    "IFS",
    "PS1",
    "PROMPT",
    "TERM",
    "SSH_AUTH_SOCK",
];

/// Where a package's `system_paths` may point: directories only root (or
/// the package manager) writes. A path in `/tmp` or a home directory could
/// be planted by someone else and would then be sourced.
pub const SYSTEM_PREFIXES: &[&str] = &["/usr/", "/opt/homebrew/", "/opt/local/", "/run/current-system/sw/", "/nix/var/nix/profiles/"];

fn is_system_path(s: &str) -> bool {
    SYSTEM_PREFIXES.iter().any(|p| s.starts_with(p)) && s[1..].split('/').all(is_plain)
}

/// A relative path inside the plugin directory, without `..` or hidden parts.
pub fn is_bundle_path(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('/') && s.split('/').all(is_plain)
}

/// Problems with an item's `os`/`shells` limits: (field, message). Each
/// must be one the plugin itself supports (`compat`).
fn scope_problems(m: &Manifest, os: &[String], shells: &[String]) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for o in os {
        if !OSES.contains(&o.as_str()) {
            out.push(("os", format!("unknown OS {o:?} ({})", OSES.join(", "))));
        } else if !m.compat.os.is_empty() && !m.compat.os.contains(o) {
            out.push(("os", format!("{o:?} isn't in compat.os")));
        }
    }
    for sh in shells {
        if !m.shells().contains(&sh.as_str()) {
            out.push(("shells", format!("{sh:?} isn't one of the shells the plugin supports ({})", m.shells().join(", "))));
        }
    }
    out
}

/// The OSes `compat.os` and the `os` limits name.
pub const OSES: &[&str] = &["linux", "macos", "windows"];

fn validate(m: &Manifest, files: Option<&BTreeSet<String>>, has_powershell_section: bool) -> Vec<Problem> {
    let mut out = Vec::new();
    let mut err = |path: String, message: String| out.push(Problem { path, message });

    if !is_id(&m.id) {
        err("id".into(), format!("{:?}: use 2–40 lowercase letters, digits and '-', starting with a letter", m.id));
    }
    if m.name.trim().is_empty() {
        err("name".into(), "empty".into());
    }
    if semver::Version::parse(&m.version).is_err() {
        err("version".into(), format!("{:?} is not a semver version (like 1.0.0)", m.version));
    }
    if m.summary.trim().is_empty() || m.summary.len() > 120 {
        err("summary".into(), "one line of 1–120 characters".into());
    }
    if m.publisher.trim().is_empty() {
        err("publisher".into(), "empty: say who maintains this plugin".into());
    }
    if let Some(h) = &m.homepage
        && !h.starts_with("https://")
    {
        err("homepage".into(), "use an https:// URL".into());
    }
    if m.category == Category::Application {
        err("category".into(), "application plugins need a runtime goatty doesn't have yet".into());
    }
    if !m.compat.goatty.is_empty() && semver::VersionReq::parse(&m.compat.goatty).is_err() {
        err("compat.goatty".into(), format!("{:?} is not a version requirement (like \">=0.1\")", m.compat.goatty));
    }
    for os in &m.compat.os {
        if !OSES.contains(&os.as_str()) {
            err("compat.os".into(), format!("unknown OS {os:?} ({})", OSES.join(", ")));
        }
    }
    for sh in &m.compat.shells {
        if !["zsh", "bash", "fish", "powershell"].contains(&sh.as_str()) {
            err("compat.shells".into(), format!("unknown shell {sh:?}"));
        }
    }
    if (has_powershell_section || !m.powershell.is_empty()) && !m.shells().contains(&"powershell") {
        err("compat.shells".into(), "include \"powershell\" in compat.shells when declaring a [powershell] section".into());
    }
    for c in &m.requires.commands {
        if !is_plain(c) {
            err("requires.commands".into(), format!("{c:?} is not a command name"));
        }
    }
    for p in &m.requires.plugins {
        if !is_id(p) || *p == m.id {
            err("requires.plugins".into(), format!("{p:?} is not another plugin's id"));
        }
    }

    // Packages.
    let mut exes = BTreeSet::new();
    let mut pkg_ids = BTreeSet::new();
    for (i, p) in m.packages.iter().enumerate() {
        let at = format!("package[{i}]");
        if !is_id(&p.id) {
            err(format!("{at}.id"), format!("{:?}: lowercase letters, digits and '-'", p.id));
        }
        if !pkg_ids.insert(&p.id) {
            err(format!("{at}.id"), format!("{:?} appears twice", p.id));
        }
        if !is_plain(&p.version) {
            err(format!("{at}.version"), format!("{:?} is not a plain version string", p.version));
        }
        if p.bin.is_empty() && p.source.is_none() {
            err(format!("{at}.bin"), "list the executables to put on PATH (or a `source` script for a shell plugin)".into());
        }
        if let Some(src) = &p.source {
            match script_shell(src).filter(|_| is_bundle_path(src)) {
                None => err(format!("{at}.source"), format!("{src:?}: a .zsh, .ps1 or .psm1 file inside the archive")),
                Some(sh) if !m.shells().contains(&sh) => {
                    err(format!("{at}.source"), format!("{src:?} is for {sh}: include \"{sh}\" in compat.shells"))
                }
                Some(_) => {}
            }
        }
        if !p.system_paths.is_empty() && p.source.as_deref().and_then(script_shell) != Some("zsh") {
            err(format!("{at}.system_paths"), "only for a package with a .zsh `source` script (programs are found on PATH)".into());
        }
        for sp in &p.system_paths {
            if !is_system_path(sp) {
                err(
                    format!("{at}.system_paths"),
                    format!("{sp:?}: an absolute path under {} without hidden or `..` parts", SYSTEM_PREFIXES.join(", ")),
                );
            }
        }
        for b in &p.bin {
            if !is_bundle_path(b) {
                err(format!("{at}.bin"), format!("{b:?}: a relative path inside the archive"));
            } else if !exes.insert(command_name(b).to_string()) {
                err(format!("{at}.bin"), format!("{:?} is provided twice", bin_name(b)));
            }
        }
        if !p.applets.is_empty() && p.bin.len() != 1 {
            err(format!("{at}.applets"), "applets are more names for one program: list exactly one bin".into());
        }
        for a in &p.applets {
            if !is_alias_name(a) || a.contains('/') {
                err(format!("{at}.applets"), format!("{a:?} is not a command name"));
            } else if !exes.insert(command_name(a).to_string()) {
                err(format!("{at}.applets"), format!("{a:?} is provided twice"));
            }
        }
        for (shell, cmd) in p.completions.commands() {
            let at = if shell.is_empty() { format!("{at}.completions") } else { format!("{at}.completions.{shell}") };
            if !shell.is_empty() && !m.shells().contains(&shell) {
                err(at.clone(), format!("{shell:?} isn't one of the shells the plugin supports ({})", m.shells().join(", ")));
            }
            if let Some(first) = cmd.first()
                && !p.bin.iter().any(|b| bin_name(b) == first || command_name(b) == first)
            {
                err(at.clone(), format!("{first:?} is not one of the package's executables"));
            }
            if cmd.iter().any(|a| a.is_empty() || a.contains('\0')) {
                err(at, "empty or invalid argument".into());
            }
        }
        if p.assets.is_empty() {
            err(format!("{at}.asset"), "no downloads".into());
        }
        let mut targets = BTreeSet::new();
        for (j, a) in p.assets.iter().enumerate() {
            let at = format!("{at}.asset[{j}]");
            if a.target != "any" && !TARGETS.contains(&a.target.as_str()) {
                err(format!("{at}.target"), format!("{:?}: one of {} or \"any\"", a.target, TARGETS.join(", ")));
            }
            if !targets.insert(&a.target) {
                err(format!("{at}.target"), format!("{:?} appears twice", a.target));
            }
            if !a.url.starts_with("https://") || a.url.chars().any(char::is_whitespace) {
                err(format!("{at}.url"), "downloads must use https://".into());
            }
            if a.sha256.len() != 64 || !a.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
                err(format!("{at}.sha256"), "64 lowercase hex digits (`goatty-plugin hash URL` prints it)".into());
            }
            if a.size == 0 {
                err(format!("{at}.size"), "the download size in bytes (`goatty-plugin hash URL` prints it)".into());
            }
            if !a.root.is_empty() && !is_bundle_path(&a.root) {
                err(format!("{at}.root"), format!("{:?}: a relative directory inside the archive", a.root));
            }
            if let Some(f) = AssetFormat::from_url(&a.url)
                && f != a.format
            {
                err(format!("{at}.format"), format!("the URL is a .{} archive: set format = \"{}\"", f.name(), f.name()));
            }
            if a.format == AssetFormat::Binary && p.bin.len() != 1 {
                err(format!("{at}.format"), "a \"binary\" download is one executable: list exactly one bin".into());
            }
        }
    }

    // Settings.
    let mut ids = BTreeSet::new();
    for (i, s) in m.settings.iter().enumerate() {
        let at = format!("setting.{}", if s.id.is_empty() { i.to_string() } else { s.id.clone() });
        if !is_setting_id(&s.id) {
            err(format!("{at}.id"), format!("{:?}: lowercase letters, digits and '_' (not \"args\"/\"value\")", s.id));
        }
        if !ids.insert(s.id.as_str()) {
            err(format!("{at}.id"), "appears twice".into());
        }
        if s.label.trim().is_empty() {
            err(format!("{at}.label"), "empty".into());
        }
        for e in crate::schema::check_definition(s) {
            err(format!("{at}.{}", e.0), e.1);
        }
        for (field, msg) in scope_problems(m, &s.os, &s.shells) {
            err(format!("{at}.{field}"), msg);
        }
        for (field, t) in [("arg", &s.arg), ("arg_off", &s.arg_off)] {
            if let Some(t) = t
                && let Err(e) = crate::template::check_inline(t, m, true)
            {
                err(format!("{at}.{field}"), e);
            }
        }
    }

    // Effects.
    let commands: BTreeSet<&str> = exes
        .iter()
        .map(String::as_str)
        .chain(m.packages.iter().flat_map(|p| p.bin.iter().map(|b| bin_name(b))))
        .chain(m.requires.commands.iter().map(String::as_str))
        .collect();
    let scope = |at: &str, os: &[String], shells: &[String], out: &mut Vec<Problem>| {
        for (field, message) in scope_problems(m, os, shells) {
            out.push(Problem { path: format!("{at}.{field}"), message });
        }
    };
    let cond = |path: String, when: &Option<String>, out: &mut Vec<Problem>| {
        if let Some(w) = when
            && let Err(e) = Condition::parse(w).and_then(|c| c.check(m))
        {
            out.push(Problem { path, message: e });
        }
    };
    let mut alias_names = BTreeSet::new();
    for (i, a) in m.aliases.iter().enumerate() {
        let at = format!("alias[{i}]");
        if !is_alias_name(&a.name) {
            out.push(Problem { path: format!("{at}.name"), message: format!("{:?} is not a usable alias name", a.name) });
        }
        if !alias_names.insert((&a.name, &a.when, &a.os, &a.shells)) {
            out.push(Problem { path: format!("{at}.name"), message: format!("{:?} is defined twice for the same condition", a.name) });
        }
        if !commands.contains(a.command.as_str()) {
            out.push(Problem {
                path: format!("{at}.command"),
                message: format!("{:?} is neither a package executable nor in requires.commands", a.command),
            });
        }
        for t in &a.args {
            if let Err(e) = crate::template::check_item(t, m) {
                out.push(Problem { path: format!("{at}.args"), message: e });
            }
        }
        cond(format!("{at}.when"), &a.when, &mut out);
        scope(&at, &a.os, &a.shells, &mut out);
    }
    for (i, e) in m.env.iter().enumerate() {
        let at = format!("env[{i}]");
        if !is_env_name(&e.name) {
            out.push(Problem { path: format!("{at}.name"), message: format!("{:?}: uppercase letters, digits and '_'", e.name) });
        } else if PROTECTED_ENV.contains(&e.name.as_str()) || e.name.starts_with("GOATTY_") || e.name.starts_with("_GOATTY") {
            out.push(Problem { path: format!("{at}.name"), message: format!("plugins may not set {}", e.name) });
        }
        if let Err(msg) = crate::template::check_inline(&e.value, m, false) {
            out.push(Problem { path: format!("{at}.value"), message: msg });
        }
        cond(format!("{at}.when"), &e.when, &mut out);
        scope(&at, &e.os, &e.shells, &mut out);
    }
    let mut file_names = BTreeSet::new();
    for (i, f) in m.files.iter().enumerate() {
        let at = format!("file[{i}]");
        if !is_plain(&f.name) {
            out.push(Problem { path: format!("{at}.name"), message: format!("{:?} is not a plain file name", f.name) });
        }
        if !file_names.insert(&f.name) {
            out.push(Problem { path: format!("{at}.name"), message: "appears twice".into() });
        }
        let shipped =
            m.zsh.init.iter().chain(&m.zsh.fpath).chain(&m.powershell.init).any(|p| *p == f.name || p.starts_with(&format!("{}/", f.name)));
        if shipped {
            out.push(Problem {
                path: format!("{at}.name"),
                message: "collides with a file the plugin ships (zsh.init / zsh.fpath / powershell.init)".into(),
            });
        }
        match f.format {
            FileFormat::Lines => {
                if f.template.is_some() {
                    out.push(Problem { path: format!("{at}.template"), message: "\"lines\" files use `lines`, not `template`".into() });
                }
                for t in &f.lines {
                    if let Err(e) = crate::template::check_item(t, m) {
                        out.push(Problem { path: format!("{at}.lines"), message: e });
                    }
                }
            }
            FileFormat::Text => match &f.template {
                None => out.push(Problem { path: format!("{at}.template"), message: "\"text\" files need a `template`".into() }),
                Some(t) => {
                    if let Err(e) = crate::template::check_inline(t, m, false) {
                        out.push(Problem { path: format!("{at}.template"), message: e });
                    }
                }
            },
        }
        if let Some(env) = &f.env
            && (!is_env_name(env) || PROTECTED_ENV.contains(&env.as_str()) || env.starts_with("GOATTY_"))
        {
            out.push(Problem { path: format!("{at}.env"), message: format!("{env:?} can't be set by a plugin") });
        }
        cond(format!("{at}.when"), &f.when, &mut out);
        scope(&at, &f.os, &f.shells, &mut out);
    }
    if let Some(init) = &m.zsh.init {
        if !is_bundle_path(init) || !init.ends_with(".zsh") {
            out.push(Problem { path: "zsh.init".into(), message: format!("{init:?}: a .zsh file inside the plugin directory") });
        } else if let Some(files) = files
            && !files.contains(init)
        {
            out.push(Problem { path: "zsh.init".into(), message: format!("{init} doesn't exist") });
        }
    }
    for d in &m.zsh.fpath {
        if !is_bundle_path(d) {
            out.push(Problem { path: "zsh.fpath".into(), message: format!("{d:?}: a directory inside the plugin directory") });
        } else if let Some(files) = files
            && !files.iter().any(|f| f.starts_with(&format!("{d}/")))
        {
            out.push(Problem { path: "zsh.fpath".into(), message: format!("{d}/ has no files") });
        }
    }
    cond("zsh.when".into(), &m.zsh.when, &mut out);
    for a in &m.zsh.after {
        if !is_id(a) || *a == m.id {
            out.push(Problem { path: "zsh.after".into(), message: format!("{a:?} is not another plugin's id") });
        }
    }
    if let Some(init) = &m.powershell.init {
        if !is_bundle_path(init) || !init.ends_with(".ps1") {
            out.push(Problem { path: "powershell.init".into(), message: format!("{init:?}: a .ps1 file inside the plugin directory") });
        } else if let Some(files) = files
            && !files.contains(init)
        {
            out.push(Problem { path: "powershell.init".into(), message: format!("{init} doesn't exist") });
        }
    }
    cond("powershell.when".into(), &m.powershell.when, &mut out);
    for a in &m.powershell.after {
        if !is_id(a) || *a == m.id {
            out.push(Problem { path: "powershell.after".into(), message: format!("{a:?} is not another plugin's id") });
        }
    }

    out.extend(crate::commands::check(m, &commands));
    for (i, c) in m.commands.iter().enumerate() {
        scope(&format!("command[{i}]"), &c.os, &c.shells, &mut out);
    }
    out.extend(crate::snippets::check(m, &commands));
    for (i, sn) in m.snippets.iter().enumerate() {
        scope(&format!("snippet[{i}]"), &sn.os, &sn.shells, &mut out);
    }
    for p in crate::permissions::check(m) {
        out.push(p);
    }
    out
}

/// When an alias, variable, file or the zsh code applies: `setting`
/// (a bool that's on), `!setting` (off), `setting=value`, `setting!=value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    On(String),
    Off(String),
    Is(String, String),
    IsNot(String, String),
}

impl Condition {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if let Some((k, v)) = s.split_once("!=") {
            return Ok(Condition::IsNot(k.trim().into(), v.trim().into()));
        }
        if let Some((k, v)) = s.split_once('=') {
            return Ok(Condition::Is(k.trim().into(), v.trim().into()));
        }
        if let Some(k) = s.strip_prefix('!') {
            return Ok(Condition::Off(k.trim().into()));
        }
        if s.is_empty() {
            return Err("empty condition".into());
        }
        Ok(Condition::On(s.into()))
    }

    /// The setting it tests.
    pub fn setting(&self) -> &str {
        match self {
            Condition::On(k) | Condition::Off(k) | Condition::Is(k, _) | Condition::IsNot(k, _) => k,
        }
    }

    /// Check it against the manifest's settings.
    pub fn check(&self, m: &Manifest) -> Result<(), String> {
        let s = m.setting(self.setting()).ok_or_else(|| format!("no setting {:?}", self.setting()))?;
        match (self, s.kind) {
            (Condition::On(_) | Condition::Off(_), SettingKind::Bool) => Ok(()),
            (Condition::On(_) | Condition::Off(_), _) => Err(format!("{} is not a bool: compare it (`{}=value`)", s.id, s.id)),
            (_, SettingKind::Bool | SettingKind::List) => Err(format!("{} can't be compared to a value", s.id)),
            (Condition::Is(_, v) | Condition::IsNot(_, v), SettingKind::Choice) if !s.choices.iter().any(|c| c.value == *v) => {
                Err(format!("{v:?} is not one of {}'s choices", s.id))
            }
            _ => Ok(()),
        }
    }

    /// Whether it holds for `values`. A setting without a value (one
    /// [`Manifest::for_platform`] left out on this host) makes any
    /// condition on it false: what depends on it isn't there either.
    pub fn eval(&self, values: &crate::schema::Values) -> bool {
        let Some(x) = values.get(self.setting()) else { return false };
        match self {
            Condition::On(_) => x.as_bool().unwrap_or(false),
            Condition::Off(_) => !x.as_bool().unwrap_or(false),
            Condition::Is(_, v) => crate::schema::plain_string(x) == *v,
            Condition::IsNot(_, v) => crate::schema::plain_string(x) != *v,
        }
    }
}

/// Evaluate an optional `when` (absent: always). Conditions were checked
/// when the manifest was parsed; a broken one counts as false.
pub fn applies(when: &Option<String>, values: &crate::schema::Values) -> bool {
    match when {
        None => true,
        Some(w) => Condition::parse(w).is_ok_and(|c| c.eval(values)),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub const EXAMPLE: &str = r#"
schema = 1
id = "eza"
name = "eza"
version = "1.0.0"
summary = "A modern ls"
publisher = "goatty"
homepage = "https://eza.rocks"

[[package]]
id = "eza"
version = "0.23.4"
bin = ["eza"]
[[package.asset]]
target = "x86_64-linux"
url = "https://example.com/eza.tar.gz"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 100

[[setting]]
id = "replace_ls"
label = "Use for ls"
type = "bool"
default = true

[[setting]]
id = "icons"
label = "Icons"
type = "choice"
default = "auto"
arg = "--icons={value}"
choices = [
  { value = "auto", label = "Automatic" },
  { value = "always", label = "Always" },
  { value = "never", label = "Never", arg = "" },
]

[[setting]]
id = "git"
label = "Git status"
type = "bool"
default = false
arg = "--git"

[[alias]]
name = "ls"
command = "eza"
args = ["{args}"]
when = "replace_ls"

[[env]]
name = "EZA_ICON_SPACING"
value = "2"
when = "icons!=never"

[permissions]
binaries = "eza is a program: goatty downloads it from its GitHub releases."
"#;

    #[test]
    fn parses_example() {
        let m = parse(EXAMPLE, None).unwrap();
        assert_eq!(m.id, "eza");
        assert_eq!(m.settings.len(), 3);
        assert_eq!(m.executables().collect::<Vec<_>>(), ["eza"]);
        assert!(m.compatible("0.0.1", "x86_64-linux").is_ok());
        assert!(m.compatible("0.0.1", "aarch64-linux").unwrap_err().contains("no build"));
    }

    fn problems(text: &str) -> Vec<String> {
        parse(text, None).unwrap_err().0.into_iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn asset_formats_match_their_urls() {
        assert_eq!(AssetFormat::from_url("https://x/fzf-darwin_arm64.zip"), Some(AssetFormat::Zip));
        assert_eq!(AssetFormat::from_url("https://x/a.TGZ?raw=1#top"), Some(AssetFormat::TarGz));
        assert_eq!(AssetFormat::from_url("https://x/a.tar.xz"), Some(AssetFormat::TarXz));
        assert_eq!(AssetFormat::from_url("https://x/a.tar.zst"), Some(AssetFormat::TarZst));
        assert_eq!(AssetFormat::from_url("https://x/a.tar"), Some(AssetFormat::Tar));
        assert_eq!(AssetFormat::from_url("https://x/download/latest"), None);
        for f in AssetFormat::ALL {
            assert_eq!(toml::Value::try_from(f).unwrap().as_str(), Some(f.name()));
        }

        let zip = EXAMPLE.replace("eza.tar.gz", "eza.zip");
        assert!(problems(&zip).iter().any(|p| p.contains("format = \"zip\"")), "{:?}", problems(&zip));
        let m = parse(&zip.replace("size = 100", "size = 100\nformat = \"zip\""), None).unwrap();
        assert_eq!(m.packages[0].assets[0].format, AssetFormat::Zip);
        // No extension: any format goes.
        let raw = EXAMPLE.replace("eza.tar.gz", "eza").replace("size = 100", "size = 100\nformat = \"binary\"");
        assert!(parse(&raw, None).is_ok());
    }

    #[test]
    fn rejects_newer_schema_and_bad_toml() {
        assert!(problems("schema = 2\nid = \"x\"")[0].contains("newer goatty"));
        assert!(problems("id = \"x\"")[0].contains("missing"));
        assert!(problems("schema = 1\nid = ")[0].contains("TOML"));
        // Unknown keys are typos, not silently ignored.
        let p = problems(&EXAMPLE.replace("homepage =", "homepag ="));
        assert!(p[0].contains("homepag"), "{p:?}");
    }

    #[test]
    fn reports_every_problem_with_its_path() {
        let bad = EXAMPLE
            .replace("id = \"eza\"\nname", "id = \"Eza!\"\nname")
            .replace("https://example.com", "http://example.com")
            .replace("when = \"replace_ls\"", "when = \"icons\"")
            .replace("name = \"EZA_ICON_SPACING\"", "name = \"LD_PRELOAD\"")
            .replace("binaries = ", "binaries_ = ");
        let p = problems(&bad);
        let has = |s: &str| p.iter().any(|x| x.contains(s));
        assert!(has("id: \"Eza!\""), "{p:?}");
        assert!(has("asset[0].url"), "{p:?}");
        assert!(has("alias[0].when: icons is not a bool"), "{p:?}");
        assert!(has("may not set LD_PRELOAD"), "{p:?}");
        assert!(has("permissions"), "{p:?}");
    }

    #[test]
    fn alias_commands_must_be_provided() {
        let p = problems(&EXAMPLE.replace("command = \"eza\"", "command = \"rm\""));
        assert!(p[0].contains("neither a package executable"), "{p:?}");
    }

    #[test]
    fn completions_run_a_package_executable() {
        let ok = EXAMPLE.replace("bin = [\"eza\"]", "bin = [\"eza\"]\ncompletions = [\"eza\", \"--completions\", \"zsh\"]");
        let m = parse(&ok, None).unwrap();
        assert_eq!(m.packages[0].completions, Completions::Command(vec!["eza".into(), "--completions".into(), "zsh".into()]));
        let p = problems(&EXAMPLE.replace("bin = [\"eza\"]", "bin = [\"eza\"]\ncompletions = [\"sh\", \"-c\", \"x\"]"));
        assert!(p.iter().any(|p| p.contains("package[0].completions") && p.contains("not one of the package's executables")), "{p:?}");
    }

    #[test]
    fn referenced_files_must_exist() {
        let text = EXAMPLE.replace(
            "[permissions]",
            "[zsh]\ninit = \"zsh/init.zsh\"\nfpath = [\"zsh/completions\"]\n\n[permissions]\nshell-code = \"defines the x function\"",
        );
        let p = parse(&text, Some(&BTreeSet::new())).unwrap_err().0;
        assert_eq!(p.len(), 2, "{p:?}");
        let files: BTreeSet<String> = ["plugin.toml", "zsh/init.zsh", "zsh/completions/_eza"].map(String::from).into();
        assert!(parse(&text, Some(&files)).is_ok());
    }

    #[test]
    fn zsh_plugin_packages_and_load_order() {
        let base = EXAMPLE
            .replace("bin = [\"eza\"]", "source = \"eza.zsh\"\nsystem_paths = [\"/usr/share/eza/eza.zsh\"]")
            .replace("[[alias]]\nname = \"ls\"\ncommand = \"eza\"\nargs = [\"{args}\"]\nwhen = \"replace_ls\"\n", "");
        let ok = format!("{base}\n[zsh]\nload = \"line-editor\"\nafter = [\"fzf\"]\n");
        let m = parse(&ok, None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(m.zsh.load, Load::LineEditor);
        assert_eq!(m.sources().map(|(p, s)| (p.id.as_str(), s)).collect::<Vec<_>>(), [("eza", "eza.zsh")]);
        assert_eq!(m.executables().count(), 0);

        for (bad, want) in
            [("/tmp/eza.zsh", "system_paths"), ("/usr/share/../../home/x.zsh", "system_paths"), ("relative/eza.zsh", "system_paths")]
        {
            let p = problems(&ok.replace("/usr/share/eza/eza.zsh", bad));
            assert!(p.iter().any(|x| x.contains(want)), "{bad}: {p:?}");
        }
        let p = problems(&ok.replace("source = \"eza.zsh\"", "source = \"eza.sh\""));
        assert!(p.iter().any(|x| x.contains("source")), "{p:?}");
        let p = problems(&ok.replace("source = \"eza.zsh\"\n", ""));
        assert!(p.iter().any(|x| x.contains("package[0].bin")), "{p:?}");
        let p = problems(&ok.replace("after = [\"fzf\"]", "after = [\"eza\"]"));
        assert!(p.iter().any(|x| x.contains("zsh.after")), "{p:?}");
        let p = problems(&ok.replace("load = \"line-editor\"", "load = \"first\""));
        assert!(p.iter().any(|x| x.contains("first")), "{p:?}");
    }

    #[test]
    fn conditions() {
        let m = parse(EXAMPLE, None).unwrap();
        let values = crate::schema::resolve(&m, &BTreeMap::new()).0;
        assert!(applies(&Some("replace_ls".into()), &values));
        assert!(!applies(&Some("!replace_ls".into()), &values));
        assert!(applies(&Some("icons=auto".into()), &values));
        assert!(applies(&Some("icons != never".into()), &values));
        assert!(Condition::parse("icons=nope").unwrap().check(&m).is_err());
        assert!(Condition::parse("missing").unwrap().check(&m).is_err());
    }

    #[test]
    fn windows_and_powershell_manifest_support() {
        let text = EXAMPLE.replace(
            "target = \"x86_64-linux\"\nurl = \"https://example.com/eza.tar.gz\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\nsize = 100",
            "target = \"x86_64-windows\"\nurl = \"https://example.com/eza.zip\"\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\nsize = 100\nformat = \"zip\"",
        )
        .replace("bin = [\"eza\"]", "bin = [\"eza.exe\"]")
        .replace(
            "[permissions]",
            "[compat]\nos = [\"windows\"]\nshells = [\"powershell\"]\n\n\
             [powershell]\ninit = \"pwsh/init.ps1\"\nload = \"prompt\"\nafter = [\"fzf\"]\n\n\
             [permissions]\nshell-code = \"sets up the eza PowerShell helper\"",
        );
        let files: BTreeSet<String> = ["plugin.toml", "pwsh/init.ps1"].map(String::from).into();
        let m = parse(&text, Some(&files)).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(m.powershell.load, Load::Prompt);
        assert_eq!(m.powershell.init.as_deref(), Some("pwsh/init.ps1"));
        assert!(m.compatible("0.1.0", "x86_64-windows").is_ok());
        assert!(m.compatible("0.1.0", "x86_64-linux").unwrap_err().contains("supports windows only"));
        // Windows on ARM takes the x64 build (emulated); Linux on ARM doesn't.
        assert!(m.compatible("0.1.0", "aarch64-windows").is_ok());
        assert_eq!(m.packages[0].asset_for("aarch64-windows").map(|a| a.target.as_str()), Some("x86_64-windows"));

        // Non-.ps1 init is rejected.
        let bad = text.replace("pwsh/init.ps1", "pwsh/init.zsh");
        assert!(problems(&bad).iter().any(|p| p.contains("powershell.init")), "{:?}", problems(&bad));
        // Declaring [powershell] without "powershell" in compat.shells is rejected at load time.
        let missing_shell = text.replace("shells = [\"powershell\"]", "shells = [\"zsh\"]");
        assert!(problems(&missing_shell).iter().any(|p| p.contains("compat.shells")), "{:?}", problems(&missing_shell));
        let omitted_shells = text.replace("shells = [\"powershell\"]\n", "");
        assert!(problems(&omitted_shells).iter().any(|p| p.contains("compat.shells")), "{:?}", problems(&omitted_shells));
        // An empty `[powershell]` section without `powershell` in `compat.shells` is also rejected.
        let empty_ps = format!("{EXAMPLE}\n[powershell]\n");
        assert!(problems(&empty_ps).iter().any(|p| p.contains("compat.shells")), "{:?}", problems(&empty_ps));
        // Missing file is rejected when file list is provided.
        let missing = parse(&text, Some(&BTreeSet::new())).unwrap_err().0;
        assert!(missing.iter().any(|p| p.path == "powershell.init"), "{missing:?}");
        assert_eq!(command_name("bin/eza.exe"), "eza");
        assert_eq!(command_name("bin/EZA.EXE"), "EZA");
        assert_eq!(command_name("starship"), "starship");
    }

    /// A plugin for zsh and PowerShell with Unix-only parts.
    const CROSS: &str = r#"
schema = 1
id = "bat"
name = "bat"
version = "1.0.0"
summary = "cat with wings"
publisher = "goatty"

[compat]
shells = ["zsh", "powershell"]

[[package]]
id = "bat"
version = "0.26.1"
bin = ["bat"]
completions = { zsh = ["bat", "--completion", "zsh"], powershell = ["bat", "--completion", "ps1"] }
[[package.asset]]
target = "x86_64-linux"
url = "https://example.com/bat.tar.gz"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 100
[[package.asset]]
target = "x86_64-windows"
url = "https://example.com/bat.zip"
sha256 = "0000000000000000000000000000000000000000000000000000000000000000"
size = 100
format = "zip"

[[setting]]
id = "man_pages"
label = "Man pages"
type = "bool"
default = true
os = ["linux", "macos"]

[[setting]]
id = "exclude"
label = "Exclude"
type = "list"
separator = "path"

[[alias]]
name = "cat"
command = "bat"

[[env]]
name = "MANPAGER"
value = "sh -c 'col -bx | bat -l man -p'"
when = "man_pages"

[[env]]
name = "BAT_EXCLUDE"
value = "{exclude}"

[[env]]
name = "BAT_PAGER"
value = "less -R"
os = ["linux", "macos"]

[[command]]
id = "pager"
title = "Page a file"
run = ["bat", "--paging=always"]
shells = ["zsh"]

[[snippet]]
id = "plain"
title = "Print plainly"
line = "bat -p"
shells = ["powershell"]

[permissions]
binaries = "bat is a program: goatty downloads it from its GitHub releases."
"#;

    #[test]
    fn narrows_to_a_platform() {
        let m = parse(CROSS, None).unwrap_or_else(|e| panic!("{e}"));
        let win = m.for_platform(&Platform::of_target("x86_64-windows"));
        assert_eq!(win.settings.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["exclude"]);
        assert_eq!(win.setting("exclude").unwrap().separator.as_deref(), Some(";"));
        assert_eq!(win.env.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["BAT_EXCLUDE"], "MANPAGER tests a Unix-only setting");
        assert!(win.commands.is_empty());
        assert_eq!(win.snippets.len(), 1);
        assert_eq!(win.packages[0].completions, Completions::Command(vec!["bat".into(), "--completion".into(), "ps1".into()]));
        assert!(win.snippets.iter().all(|s| s.shells.is_empty() && s.os.is_empty()), "no limits left for older clients");

        let linux = m.for_platform(&Platform::of_target("x86_64-linux"));
        assert_eq!(linux.settings.len(), 2);
        assert_eq!(linux.setting("exclude").unwrap().separator.as_deref(), Some(":"));
        assert_eq!(linux.env.len(), 3);
        assert_eq!(linux.commands.len(), 1);
        assert!(linux.snippets.is_empty());
        assert!(linux.settings.iter().all(|s| s.os.is_empty()));

        // What's left serializes without the new fields.
        let json = serde_json::to_string(&win).unwrap();
        assert_eq!(json.matches("\"shells\"").count(), 1, "only compat.shells: {json}");
        assert!(!json.contains("\"os\":[\"linux"), "{json}");
        assert!(!json.contains("\"powershell\":["), "no per-shell completions: {json}");
        assert!(!json.contains("\"man_pages\""), "{json}");

        // A setting a host doesn't have makes conditions on it false.
        let values = crate::schema::resolve(&win, &BTreeMap::new()).0;
        assert!(!applies(&Some("man_pages".into()), &values));
        assert!(!applies(&Some("!man_pages".into()), &values));
    }

    #[test]
    fn completions_per_shell() {
        let list = |v: &[&str]| Completions::Command(v.iter().map(|s| (*s).to_owned()).collect());
        assert_eq!(list(&["fd", "--gen-completions", "zsh"]).for_shell("powershell").unwrap(), ["fd", "--gen-completions", "powershell"]);
        assert_eq!(
            list(&["rg", "--generate", "complete-zsh"]).for_shell("powershell").unwrap(),
            ["rg", "--generate", "complete-powershell"]
        );
        assert_eq!(list(&["x", "--shell=zsh"]).for_shell("powershell").unwrap(), ["x", "--shell=powershell"]);
        assert_eq!(list(&["x", "zshrc", "nozsh"]).for_shell("powershell").unwrap(), ["x", "zshrc", "nozsh"]);
        assert_eq!(list(&["rg", "--generate", "complete-zsh"]).for_shell("zsh").unwrap(), ["rg", "--generate", "complete-zsh"]);
        assert_eq!(list(&[]).for_shell("zsh"), None);
        let m = parse(CROSS, None).unwrap();
        assert_eq!(m.packages[0].completions.for_shell("zsh").unwrap(), ["bat", "--completion", "zsh"]);

        let p = problems(&CROSS.replace("powershell = [\"bat\"", "fish = [\"bat\""));
        assert!(p.iter().any(|p| p.contains("completions.fish") && p.contains("isn't one of the shells")), "{p:?}");
        let p = problems(&CROSS.replace("powershell = [\"bat\", \"--completion\"", "powershell = [\"cat\", \"--completion\""));
        assert!(p.iter().any(|p| p.contains("completions.powershell") && p.contains("not one of the package's executables")), "{p:?}");
    }

    #[test]
    fn scopes_must_be_ones_the_plugin_supports() {
        let p = problems(&CROSS.replace("os = [\"linux\", \"macos\"]\n\n[[setting]]", "os = [\"bsd\"]\n\n[[setting]]"));
        assert!(p.iter().any(|p| p.contains("setting.man_pages.os") && p.contains("unknown OS")), "{p:?}");
        let p = problems(&CROSS.replace("shells = [\"zsh\"]\n\n[[snippet]]", "shells = [\"fish\"]\n\n[[snippet]]"));
        assert!(p.iter().any(|p| p.contains("command[0].shells")), "{p:?}");
        let p = problems(&CROSS.replace("shells = [\"zsh\", \"powershell\"]", "os = [\"windows\"]\nshells = [\"zsh\", \"powershell\"]"));
        assert!(p.iter().any(|p| p.contains("setting.man_pages.os") && p.contains("isn't in compat.os")), "{p:?}");
        // The same alias name for different OSes is fine.
        let twice = CROSS.replace(
            "[[alias]]\nname = \"cat\"\ncommand = \"bat\"\n",
            "[[alias]]\nname = \"cat\"\ncommand = \"bat\"\nos = [\"windows\"]\n\n[[alias]]\nname = \"cat\"\ncommand = \"bat\"\nos = [\"linux\"]\n",
        );
        assert!(parse(&twice, None).is_ok(), "{:?}", parse(&twice, None));
    }

    #[test]
    fn applets_and_powershell_scripts() {
        let text = CROSS.replace("bin = [\"bat\"]", "bin = [\"bat\"]\napplets = [\"cat\", \"less\"]");
        let m = parse(&text, None).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(m.executables().collect::<Vec<_>>(), ["bat", "cat", "less"]);
        let p = problems(&text.replace("bin = [\"bat\"]", "bin = [\"bat\", \"batman\"]"));
        assert!(p.iter().any(|p| p.contains("applets") && p.contains("exactly one bin")), "{p:?}");
        let p = problems(&text.replace("\"less\"]", "\"bat\"]"));
        assert!(p.iter().any(|p| p.contains("applets") && p.contains("provided twice")), "{p:?}");
        let p = problems(&text.replace("\"less\"]", "\"a/b\"]"));
        assert!(p.iter().any(|p| p.contains("applets") && p.contains("not a command name")), "{p:?}");

        // A PowerShell script package is PowerShell's only.
        let script = EXAMPLE
            .replace("bin = [\"eza\"]", "source = \"tips.psm1\"")
            .replace("[[alias]]\nname = \"ls\"\ncommand = \"eza\"\nargs = [\"{args}\"]\nwhen = \"replace_ls\"\n", "");
        let p = problems(&script);
        assert!(p.iter().any(|p| p.contains("source") && p.contains("include \"powershell\"")), "{p:?}");
        let ok = script.replace("[permissions]", "[compat]\nshells = [\"zsh\", \"powershell\"]\n\n[permissions]");
        let m = parse(&ok, None).unwrap_or_else(|e| panic!("{e}"));
        assert!(m.packages[0].applies_to_shell("powershell") && !m.packages[0].applies_to_shell("zsh"));
        assert_eq!(m.sources_for("zsh").count(), 0);
        assert!(m.for_platform(&Platform::of_target("x86_64-linux")).packages.is_empty());
        let p = problems(&ok.replace("source = \"tips.psm1\"", "source = \"tips.psm1\"\nsystem_paths = [\"/usr/share/tips.psm1\"]"));
        assert!(p.iter().any(|p| p.contains("system_paths")), "{p:?}");
    }

    #[test]
    fn ids() {
        assert!(is_id("eza") && is_id("zsh-you-should-use") && is_id("a1"));
        assert!(!is_id("a") && !is_id("Eza") && !is_id("-x") && !is_id("x-") && !is_id("1x") && !is_id("a_b"));
        assert!(is_bundle_path("zsh/init.zsh") && !is_bundle_path("../x") && !is_bundle_path("/etc") && !is_bundle_path("a/.hidden"));
    }
}

//! goatty plugins (Stage 5): the format and the trust machinery, without
//! any knowledge of where goatty keeps things on a host (that's
//! `goatty-shell::plugins`).
//!
//! * [`manifest`]: `plugin.toml`: identity, packages (pinned downloads),
//!   settings, aliases, environment, config files, zsh code,
//! * [`schema`]: settings values: defaults, validation, lenient resolution,
//! * [`commands`]: palette commands that ask for input and type a command line,
//! * [`snippets`]: command lines with blanks, suggestion sources, expansions,
//! * [`alias`]: a plugin's aliases as rows for the GUI and CLI,
//! * [`template`]: how setting values reach aliases, variables and files,
//! * [`permissions`]: what a plugin may do, derived from its manifest,
//! * [`zsh`]: a plugin's shell integration, rendered and quoted,
//! * [`bundle`]: a plugin directory as checksummed files,
//! * [`index`] + [`sign`]: registries, their signed index, verification,
//! * [`fetch`]: HTTPS downloads through curl/wget,
//! * [`archive`]: unpacking package downloads (tar.gz/xz/zst, zip),
//! * [`api`]: plugin operations as JSON (protocol, GUI),
//! * [`changes`]: what an update changes, for a review that shows only that.
//!
//! The `goatty-plugin` binary wraps these for plugin authors (`new`,
//! `validate`, `render`, `hash`) and registry maintainers (`index build`,
//! `index sign`, `index verify`, `keygen`). See `docs/plugins.md`.

pub mod alias;
pub mod api;
pub mod archive;
pub mod bundle;
pub mod changes;
pub mod commands;
pub mod fetch;
pub mod index;
pub mod manifest;
pub mod permissions;
pub mod schema;
pub mod sign;
pub mod snippets;
pub mod template;
pub mod zsh;

pub use manifest::Manifest;
pub use permissions::Permission;

/// The goatty version plugins are checked against (`compat.goatty`).
pub const GOATTY_VERSION: &str = env!("CARGO_PKG_VERSION");

/// `"<arch>-<os>"` of this build (`x86_64-linux`).
pub fn current_target() -> String {
    format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

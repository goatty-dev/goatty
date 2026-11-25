//! `goatty-plugin`: tools for plugin authors and registry maintainers.
//! Installing plugins is `goattyd plugin …` (or the settings window).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use goatty_plugin::{bundle, index, manifest, permissions, powershell, schema, sign, snippets, zsh};

#[derive(Parser)]
#[command(name = "goatty-plugin", version, about = "Build, check and publish goatty plugins")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start a new plugin in DIR (default: ./ID).
    New { id: String, dir: Option<PathBuf> },
    /// Check a plugin directory (default: .); exit status 1 on problems.
    Validate { dir: Option<PathBuf> },
    /// Print the shell code and config files goatty would generate.
    Render {
        dir: Option<PathBuf>,
        /// A setting value: KEY=VALUE (repeatable).
        #[arg(long = "set", value_name = "KEY=VALUE")]
        set: Vec<String>,
        /// Target shell (`zsh` or `powershell`).
        #[arg(long, default_value = "zsh")]
        shell: String,
    },
    /// Print the sha256 and size of downloads (URLs) or files, for [[package.asset]].
    Hash {
        #[arg(required = true)]
        sources: Vec<String>,
    },
    /// Download every asset of the plugin(s) and check sha256 and size (DIRs default: .).
    CheckDownloads { dirs: Vec<PathBuf> },
    /// Registry maintenance.
    #[command(subcommand)]
    Index(IndexCmd),
    /// Create a signing key (secret, mode 0600) and print its public key.
    Keygen { file: PathBuf },
}

#[derive(Subcommand)]
enum IndexCmd {
    /// Validate every plugin and write index.json (default DIR: .).
    Build { dir: Option<PathBuf> },
    /// Sign index.json with a key listed in registry.toml.
    Sign {
        dir: Option<PathBuf>,
        #[arg(long)]
        key: PathBuf,
    },
    /// Check the signature (by one of --trust, default registry.toml's keys) and every file.
    Verify {
        dir: Option<PathBuf>,
        /// Public key (hex) to trust (repeatable).
        #[arg(long = "trust")]
        trust: Vec<String>,
    },
    /// Exit status 1 when index.json doesn't match the plugin directories.
    Check { dir: Option<PathBuf> },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn dir_or_cwd(d: Option<PathBuf>) -> PathBuf {
    d.unwrap_or_else(|| PathBuf::from("."))
}

fn run(cli: Cli) -> Result<bool> {
    match cli.cmd {
        Cmd::New { id, dir } => new(&id, &dir.unwrap_or_else(|| PathBuf::from(&id))).map(|()| true),
        Cmd::Validate { dir } => validate(&dir_or_cwd(dir)),
        Cmd::Render { dir, set, shell } => render(&dir_or_cwd(dir), &set, &shell).map(|()| true),
        Cmd::Hash { sources } => hash(&sources).map(|()| true),
        Cmd::CheckDownloads { dirs } => check_downloads(&if dirs.is_empty() { vec![PathBuf::from(".")] } else { dirs }),
        Cmd::Keygen { file } => {
            let public = sign::keygen_file(&file)?;
            println!("secret key written to {} (mode 0600). Keep it offline; never commit it.", file.display());
            println!("public key: {public}");
            println!("add the public key to registry.toml's keys, and to the clients that should trust this registry.");
            Ok(true)
        }
        Cmd::Index(cmd) => match cmd {
            IndexCmd::Build { dir } => {
                let dir = dir_or_cwd(dir);
                let idx = index::build(&dir)?;
                let versions: usize = idx.plugins.values().map(|e| e.versions.len()).sum();
                std::fs::write(dir.join(index::INDEX), idx.to_bytes())?;
                println!("index.json: {} plugins, {versions} versions, sequence {}, {}", idx.plugins.len(), idx.sequence, expiry(&idx));
                println!("sign it next: goatty-plugin index sign --key <secret key file>");
                Ok(true)
            }
            IndexCmd::Sign { dir, key } => {
                let dir = dir_or_cwd(dir);
                let key = sign::load_secret(&key)?;
                if !index::is_current(&dir)? {
                    bail!("index.json is out of date: run `goatty-plugin index build` first");
                }
                let sig = index::sign_dir(&dir, &key)?;
                println!("signed index.json with {}", sig.key);
                Ok(true)
            }
            IndexCmd::Verify { dir, trust } => {
                let dir = dir_or_cwd(dir);
                let trusted = if trust.is_empty() { index::read_meta(&dir)?.keys } else { trust };
                let idx = index::verify_snapshot(&dir, &trusted, 0)?;
                println!(
                    "index.json: signature ok, {} plugins, every file matches (sequence {}, {})",
                    idx.plugins.len(),
                    idx.sequence,
                    expiry(&idx)
                );
                Ok(true)
            }
            IndexCmd::Check { dir } => {
                let dir = dir_or_cwd(dir);
                let current = index::is_current(&dir)?;
                let committed: Option<index::Index> =
                    std::fs::read(dir.join(index::INDEX)).ok().and_then(|b| serde_json::from_slice(&b).ok());
                if current {
                    println!("index.json is current");
                } else if let Some(c) = committed.filter(|c| index::build(&dir).is_ok_and(|b| b.plugins == c.plugins)) {
                    let why = if c.expires.is_some() { expiry(&c) } else { "it has no expiry yet".into() };
                    println!("index.json is due for renewal ({why}): run `goatty-plugin index build` and sign it");
                } else {
                    println!("index.json doesn't match the plugins: run `goatty-plugin index build` and sign it");
                }
                Ok(current)
            }
        },
    }
}

/// "expires in N days" (or "expired N days ago", "never expires").
fn expiry(idx: &index::Index) -> String {
    let Some(expires) = idx.expires else { return "never expires".into() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    const DAY: u64 = 24 * 60 * 60;
    if idx.expired(now) {
        format!("expired {} days ago", (now - expires) / DAY)
    } else {
        format!("expires in {} days", (expires - now) / DAY)
    }
}

fn validate(dir: &Path) -> Result<bool> {
    let m = match manifest::load_dir(dir) {
        Ok(m) => m,
        Err(e) => {
            println!("{e:#}");
            return Ok(false);
        }
    };
    let files = bundle::list_files(dir)?;
    println!("{} {} ({}): valid", m.name, m.version, m.id);
    println!("  files       {}", files.keys().cloned().collect::<Vec<_>>().join(", "));
    for p in &m.packages {
        let targets: Vec<&str> = p.assets.iter().map(|a| a.target.as_str()).collect();
        println!("  package     {} {} → {} ({})", p.id, p.version, p.bin.join(", "), targets.join(", "));
    }
    println!("  settings    {}", m.settings.iter().map(|s| s.id.as_str()).collect::<Vec<_>>().join(", "));
    for g in permissions::describe(&m) {
        let reason = if g.reason.is_empty() { String::new() } else { format!(": {}", g.reason) };
        println!("  permission  {}{reason}", g.permission.key());
        if !g.details.is_empty() {
            println!("              {}", g.details.join(", "));
        }
    }
    for sn in &m.snippets {
        println!("  snippet     {} [{}]: {}", sn.title, sn.safety().key(), sn.line);
    }
    for e in &m.expansions {
        println!("  expansion   {} → {}", e.say, e.to);
    }
    let target = goatty_plugin::current_target();
    if let Err(why) = m.compatible(goatty_plugin::GOATTY_VERSION, &target) {
        println!("  note        not installable on this machine ({target}): {why}");
    }
    for s in m.settings.iter().filter(|s| s.help.trim().is_empty()) {
        println!("  hint        setting {:?} has no help text: explain what it does in plain words", s.id);
    }
    for s in m.snippets.iter().filter(|s| s.description.trim().is_empty()) {
        println!("  hint        snippet {:?} has no description: say in plain words what happens when it runs", s.id);
    }
    for s in m.snippets.iter().filter(|s| s.safety.is_none()) {
        println!("  hint        snippet {:?} doesn't say its safety; goatty guesses {:?}", s.id, s.safety().key());
    }
    Ok(true)
}

fn render(dir: &Path, set: &[String], shell: &str) -> Result<()> {
    let m = manifest::load_dir(dir)?;
    let mut stored = BTreeMap::new();
    for kv in set {
        let (k, v) = kv.split_once('=').with_context(|| format!("{kv:?}: use KEY=VALUE"))?;
        let s = m.setting(k).with_context(|| format!("no setting {k:?}"))?;
        stored.insert(k.to_owned(), schema::parse_input(s, v).map_err(anyhow::Error::msg)?);
    }
    let (values, issues) = schema::resolve(&m, &stored);
    for (k, msg) in issues {
        eprintln!("warning: {k}: {msg}");
    }
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).ok();
    match shell {
        "zsh" => {
            let r = zsh::render(
                &m,
                &values,
                &zsh::Ctx {
                    dir: format!("$_goatty_dir/plugins/{}", m.id),
                    home: home.as_deref(),
                    label: format!("{} {}", m.id, m.version),
                    feature: zsh::feature(&m.id),
                    packages: "$_goatty_pkgs".into(),
                },
            );
            println!("### plugins-early.zsh (before compinit)\n{}", if r.early.is_empty() { "(nothing)\n" } else { &r.early });
            println!("### {} (load = \"{}\")\n{}", r.load.file(), r.load.key(), r.main);
            for (name, contents) in &r.files {
                println!("### plugins/{}/{name}\n{contents}", m.id);
            }
        }
        "powershell" | "pwsh" => {
            let r = powershell::render(
                &m,
                &values,
                &powershell::Ctx {
                    dir: format!("(Join-Path $_goatty_dir {})", powershell::quote(&format!("plugins/{}", m.id))),
                    home: home.as_deref(),
                    label: format!("{} {}", m.id, m.version),
                    feature: zsh::feature(&m.id),
                },
            );
            println!("### plugins-early.ps1\n{}", if r.early.is_empty() { "(nothing)\n" } else { &r.early });
            println!("### {} (load = \"{}\")\n{}", r.load.ps_file(), r.load.key(), r.main);
            for (name, contents) in &r.files {
                println!("### plugins/{}/{name}\n{contents}", m.id);
            }
        }
        other => bail!("unknown shell {other:?}: use `zsh` or `powershell`"),
    }
    if !m.snippets.is_empty() {
        println!("### snippets (what the picker inserts, with each input's default or ‹label›)");
        for sn in &m.snippets {
            let values: snippets::Answers = sn.inputs.iter().filter_map(|i| i.default.clone().map(|d| (i.id.clone(), d))).collect();
            let shown: String = sn.preview(&values, None).iter().map(snippets::Seg::text).collect();
            println!("{}: {shown}", sn.title);
        }
        println!();
    }
    if !m.expansions.is_empty() {
        let rules: Vec<snippets::Compiled> = m.expansions.iter().filter_map(|e| snippets::compile(e).ok()).collect();
        if matches!(shell, "powershell" | "pwsh") {
            println!("### expansions.ps1\n{}", snippets::render_table_powershell(&rules));
        } else {
            println!("### expansions.zsh\n{}", snippets::render_table(&rules));
        }
    }
    Ok(())
}

fn hash(sources: &[String]) -> Result<()> {
    let cache = std::env::temp_dir().join(format!("goatty-plugin-{}", std::process::id()));
    for src in sources {
        let bytes = if src.starts_with("https://") {
            goatty_plugin::fetch::download(src, &cache)?
        } else {
            std::fs::read(src).with_context(|| format!("read {src}"))?
        };
        println!("{src}\n  sha256 = \"{}\"\n  size = {}", bundle::sha256_hex(&bytes), bytes.len());
        if let Some(format) = manifest::AssetFormat::from_url(src) {
            if format != manifest::AssetFormat::TarGz {
                println!("  format = \"{}\"", format.name());
            }
            for path in goatty_plugin::archive::executables(format, &bytes, 10)? {
                println!("  executable in archive: {path}");
            }
        }
    }
    let _ = std::fs::remove_dir_all(&cache);
    Ok(())
}

fn check_downloads(dirs: &[PathBuf]) -> Result<bool> {
    let cache = std::env::temp_dir().join(format!("goatty-plugin-{}", std::process::id()));
    let mut ok = true;
    for dir in dirs {
        let m = manifest::load_dir(dir)?;
        for p in &m.packages {
            for a in &p.assets {
                let result = goatty_plugin::fetch::download(&a.url, &cache).and_then(|bytes| {
                    let sha = bundle::sha256_hex(&bytes);
                    if sha != a.sha256 {
                        bail!("sha256 is {sha}, the manifest says {}", a.sha256);
                    }
                    if bytes.len() as u64 != a.size {
                        bail!("size is {}, the manifest says {}", bytes.len(), a.size);
                    }
                    Ok(())
                });
                match result {
                    Ok(()) => println!("ok    {} {} {}", m.id, a.target, a.url),
                    Err(e) => {
                        ok = false;
                        println!("FAIL  {} {} {}: {e:#}", m.id, a.target, a.url);
                    }
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&cache);
    Ok(ok)
}

fn new(id: &str, dir: &Path) -> Result<()> {
    if !manifest::is_id(id) {
        bail!("{id:?}: use 2–40 lowercase letters, digits and '-', starting with a letter");
    }
    if dir.join(manifest::FILE).exists() {
        bail!("{} already has a plugin.toml", dir.display());
    }
    std::fs::create_dir_all(dir)?;
    let text = SKELETON.replace("{{id}}", id);
    std::fs::write(dir.join(manifest::FILE), text)?;
    println!("created {}", dir.join(manifest::FILE).display());
    println!("next: fill in [[package.asset]] with `goatty-plugin hash <url>`, then `goatty-plugin validate {}`", dir.display());
    println!("the template repository has a complete, commented example: https://github.com/goatty-dev/goatty-plugin-template");
    Ok(())
}

const SKELETON: &str = r#"# A goatty plugin. Format: https://github.com/goatty-dev/goatty/blob/main/docs/plugins.md
schema = 1
id = "{{id}}"
name = "{{id}}"
version = "0.1.0"
summary = "One line: what the tool does"
description = """
A few sentences for the details view: what the tool is, and why someone would want it.
"""
publisher = "your-github-name"
homepage = "https://example.com"
license = "MIT"
keywords = []

# The program goatty downloads into its own directory (never system-wide).
[[package]]
id = "{{id}}"
version = "1.0.0"
bin = ["{{id}}"]

[[package.asset]]
target = "x86_64-linux"
url = "https://github.com/OWNER/REPO/releases/download/v1.0.0/{{id}}-x86_64-unknown-linux-musl.tar.gz"
sha256 = "run: goatty-plugin hash <url>"
size = 0
root = ""

# A setting shown in goatty's settings window. Explain it in plain words.
[[setting]]
id = "example"
label = "Example option"
help = "What this changes, for someone who has never used the tool."
type = "bool"
default = true
arg = "--example"

[[alias]]
name = "{{id}}"
command = "{{id}}"
args = ["{args}"]
description = "Runs {{id}} with the options chosen in goatty's settings"

[permissions]
binaries = "{{id}} is a program: goatty downloads it from the project's GitHub releases."
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn skeleton_parses_once_filled_in() {
        let filled =
            SKELETON.replace("{{id}}", "demo").replace("run: goatty-plugin hash <url>", &"a".repeat(64)).replace("size = 0", "size = 1");
        manifest::parse(&filled, None).unwrap();
    }
}

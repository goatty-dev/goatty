//! A plugin's PowerShell integration, rendered from its manifest and settings (#278).
//!
//! Mirrors [`crate::zsh`] for Windows managed PowerShell (`pwsh.exe` 7 /
//! Windows PowerShell 5.1):
//!
//! * **early** (`plugins-early.ps1`): exported environment variables and
//!   generated config file paths;
//! * **main** (in the file of the plugin's [`Load`] slot, e.g.
//!   `plugins.ps1`): aliases (as `global:` functions, removing any built-in
//!   PowerShell alias of the same name), the plugin's `[powershell].init`
//!   script, guarded on its required commands being installed.
//!
//! Everything goatty writes is quoted with [`quote`]; the only code a plugin
//! brings is its reviewed `[powershell].init` file (`shell-code` permission).

use serde_json::Value;

use crate::manifest::{FileFormat, Load, Manifest, SettingKind, applies, command_name};
use crate::schema::{Values, plain_string};
use crate::template::{expand_inline, expand_list};

/// Characters that PowerShell's parser treats as single-quote delimiters in
/// addition to ASCII `'` (Unicode left/right/low/reversed single quotes).
pub(crate) fn is_ps_single_quote(c: char) -> bool {
    matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}')
}

/// Characters that PowerShell's parser treats as double-quote delimiters in
/// addition to ASCII `"` (Unicode left/right/low double quotes).
pub(crate) fn is_ps_double_quote(c: char) -> bool {
    matches!(c, '"' | '\u{201C}' | '\u{201D}' | '\u{201E}')
}

/// Whether `s` needs legacy native-argument escaping on Windows PowerShell 5.1
/// (and PowerShell < 7.3 / `Legacy` mode), where embedded double quotes or a
/// trailing backslash in a whitespace-containing argument are mangled before
/// reaching Win32 `CommandLineToArgvW`.
pub(crate) fn needs_legacy_native_escape(s: &str) -> bool {
    s.contains('"') || (s.ends_with('\\') && s.chars().any(char::is_whitespace))
}

/// Escape `s` for Win32 `CommandLineToArgvW` when passed through legacy
/// PowerShell native argument serialization (PowerShell 5.1 / < 7.3).
pub(crate) fn escape_legacy_native_arg(s: &str) -> String {
    let has_ws = s.chars().any(char::is_whitespace);
    let mut out = String::with_capacity(s.len() + 4);
    let mut backslashes = 0usize;
    for c in s.chars() {
        if c == '\\' {
            backslashes += 1;
        } else if c == '"' {
            for _ in 0..(backslashes * 2 + 1) {
                out.push('\\');
            }
            out.push('"');
            backslashes = 0;
        } else {
            for _ in 0..backslashes {
                out.push('\\');
            }
            backslashes = 0;
            out.push(c);
        }
    }
    let trailing = if has_ws { backslashes * 2 } else { backslashes };
    for _ in 0..trailing {
        out.push('\\');
    }
    out
}

/// PowerShell expression testing whether the active host uses legacy native
/// command argument passing (Windows PowerShell 5.1, PowerShell 7.0–7.2, or
/// PowerShell 7.3+ configured with `$PSNativeCommandArgumentPassing = 'Legacy'`).
pub(crate) const LEGACY_NATIVE_COND: &str = "$PSVersionTable.PSVersion.Major -lt 7 -or ($PSVersionTable.PSVersion.Major -eq 7 -and $PSVersionTable.PSVersion.Minor -lt 3) -or $PSNativeCommandArgumentPassing -eq 'Legacy'";

/// Quote a PowerShell argument for a native executable, preserving embedded
/// double quotes on both PowerShell 7.3+ and legacy Windows PowerShell 5.1.
///
/// Ordinary arguments without embedded `"` (or trailing `\` with whitespace)
/// are rendered identically to [`word`].
pub fn native_word(s: &str) -> String {
    if !needs_legacy_native_escape(s) {
        return word(s);
    }
    let legacy = quote(&escape_legacy_native_arg(s));
    let modern = quote(s);
    format!("$(if ({LEGACY_NATIVE_COND}) {{ {legacy} }} else {{ {modern} }})")
}

/// Whether `prog` looks like a PowerShell `Verb-Noun` cmdlet (e.g.
/// `Write-Output`, `Set-Location`, `Get-Content`) or built-in cmdlet alias
/// whose arguments are bound directly as .NET strings without Win32 native
/// command-line serialization.
pub(crate) fn is_ps_cmdlet(prog: &str) -> bool {
    const VERBS: &[&str] = &[
        "add",
        "clear",
        "close",
        "convertfrom",
        "convertto",
        "copy",
        "disable",
        "enable",
        "export",
        "foreach",
        "format",
        "get",
        "group",
        "hide",
        "import",
        "invoke",
        "join",
        "measure",
        "move",
        "new",
        "open",
        "out",
        "pop",
        "push",
        "read",
        "register",
        "remove",
        "rename",
        "resolve",
        "restart",
        "select",
        "set",
        "show",
        "sort",
        "split",
        "start",
        "stop",
        "test",
        "unregister",
        "update",
        "where",
        "write",
    ];
    if prog.eq_ignore_ascii_case("echo") {
        return true;
    }
    if let Some((verb, noun)) = prog.split_once('-') {
        return !noun.is_empty() && noun.bytes().all(|b| b.is_ascii_alphabetic()) && VERBS.iter().any(|v| v.eq_ignore_ascii_case(verb));
    }
    false
}

/// Quote a string as a PowerShell literal expression.
///
/// Ordinary strings become `'...'` with ASCII `'` doubled as `''`. Strings
/// containing Unicode curly single quotes (which PowerShell's parser also
/// treats as single-quote delimiters) or control characters are constructed
/// via literal concatenation with `[char]0xNNNN` so untrusted values can never
/// break out of quoting.
pub fn quote(s: &str) -> String {
    let needs_concat = s.chars().any(|c| (is_ps_single_quote(c) && c != '\'') || c.is_control());
    if !needs_concat {
        return format!("'{}'", s.replace('\'', "''"));
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, parts: &mut Vec<String>| {
        if !cur.is_empty() {
            parts.push(format!("'{}'", cur.replace('\'', "''")));
            cur.clear();
        }
    };
    for c in s.chars() {
        if (is_ps_single_quote(c) && c != '\'') || c.is_control() {
            flush(&mut cur, &mut parts);
            parts.push(format!("[char]0x{:04x}", c as u32));
        } else {
            cur.push(c);
        }
    }
    flush(&mut cur, &mut parts);
    if parts.is_empty() {
        return "''".to_owned();
    }
    if parts.len() == 1 { parts.pop().unwrap_or_else(|| "''".to_owned()) } else { format!("({})", parts.join(" + ")) }
}

/// Quote a PowerShell argument only when needed (readable commands: `eza --icons=auto`).
///
/// Excludes PowerShell metacharacters (`,`, `;`, `@`, `#`, `$`, `&`, `%`, `:`,
/// `|`, `<`, `>`, `(`, `)`, `{`, `}`, `[`, `]`, `'`, `"`, `` ` ``, `~`, `*`, `?`,
/// whitespace, and leading `.`).
pub fn word(s: &str) -> String {
    if !s.is_empty()
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./=-+\\".contains(&b))
        && !s.starts_with('.')
        && !s.starts_with('/')
    {
        s.to_owned()
    } else {
        quote(s)
    }
}

/// Format the command (first word) of a PowerShell command line: uses `&` call
/// operator whenever the program name requires quoting so PowerShell executes
/// it instead of echoing a string literal.
pub fn command_word(s: &str) -> String {
    let w = word(s);
    if w == s { w } else { format!("& {w}") }
}

/// Where things are, as seen from the generated PowerShell files.
pub struct Ctx<'a> {
    /// PowerShell expression evaluating to the plugin's directory in the
    /// generation, e.g. `(Join-Path $_goatty_dir 'plugins/eza')`.
    pub dir: String,
    /// Home directory on the host (`USERPROFILE`), for `~` in path settings.
    pub home: Option<&'a str>,
    /// Shown in the generated comment: `eza 1.0.0 from official`.
    pub label: String,
    /// The key in `goatty-status` (`plugin:eza`; built-in plugins use their id).
    pub feature: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rendered {
    pub early: String,
    pub main: String,
    /// Generated config files: (name inside the plugin's directory, contents).
    pub files: Vec<(String, String)>,
    /// Plugin init script referenced, if any.
    pub uses_init: Option<String>,
    /// The slot `main` goes into.
    pub load: Load,
}

/// `~`, `~/x`, and `~\x` in path settings and list entries become `home`.
pub fn expand_home(m: &Manifest, values: &mut Values, home: Option<&str>) {
    let Some(home) = home else { return };
    let sep = if home.contains('\\') { '\\' } else { '/' };
    let trimmed = home.trim_end_matches(['/', '\\']);
    let expand = |p: &mut String| {
        if p == "~" {
            *p = home.to_owned();
        } else if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix(r"~\")) {
            *p = format!("{trimmed}{sep}{rest}");
        }
    };
    for s in &m.settings {
        match (s.kind, values.get_mut(&s.id)) {
            (SettingKind::Path, Some(Value::String(p))) => expand(p),
            (SettingKind::List, Some(Value::Array(items))) => items
                .iter_mut()
                .filter_map(|i| match i {
                    Value::String(p) => Some(p),
                    _ => None,
                })
                .for_each(expand),
            _ => {}
        }
    }
}

/// Whether `name` is safe as an unquoted PowerShell function identifier after `global:`.
fn is_simple_ps_ident(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Render a PowerShell alias definition that removes any conflicting built-in
/// PowerShell alias and defines a `global:` function invoking the external
/// `command` with `args` plus any forwarded `@args` (without recursing if the
/// alias has the same name as `command`).
pub fn render_alias_fn(name: &str, command: &str, args: &[String]) -> String {
    let alias_path = quote(&format!("Alias:{name}"));
    let remove = format!(
        "if (Test-Path -LiteralPath {alias_path}) {{ Remove-Item -LiteralPath {alias_path} -Force -ErrorAction SilentlyContinue }}"
    );
    let cmd_q = quote(command);
    let mut call = format!("& (Get-Command -Name {cmd_q} -CommandType Application -ErrorAction Stop | Select-Object -First 1).Source");
    for arg in args {
        call.push(' ');
        call.push_str(&word(arg));
    }
    call.push_str(" @args");
    if is_simple_ps_ident(name) {
        format!("{remove}\nfunction global:{name} {{ {call} }}")
    } else {
        let fn_path = quote(&format!("Function:global:{name}"));
        format!("{remove}\nSet-Item -LiteralPath {fn_path} -Value {{ {call} }}")
    }
}

/// Effective load slot for PowerShell: `[powershell].load` when `[powershell]`
/// sets one (or is present), falling back to `[zsh].load` for purely
/// declarative plugins that only set load order in `[zsh]`.
pub fn load_slot(m: &Manifest) -> Load {
    if !m.powershell.is_empty() { m.powershell.load } else { m.zsh.load }
}

/// Effective `after` list for PowerShell.
pub fn after_list(m: &Manifest) -> &[String] {
    if !m.powershell.is_empty() { &m.powershell.after } else { &m.zsh.after }
}

pub fn render(m: &Manifest, values: &Values, ctx: &Ctx<'_>) -> Rendered {
    let mut values = values.clone();
    expand_home(m, &mut values, ctx.home);
    let values = &values;
    let mut r = Rendered::default();

    // --- early: environment variables and generated config files ------------
    let mut early = Vec::new();
    for e in m.env.iter().filter(|e| applies(&e.when, values)) {
        let v = expand_inline(&e.value, m, values, None);
        if !v.is_empty() {
            early.push(format!("Set-Item -Path {} -Value {}", quote(&format!("Env:{}", e.name)), quote(&v)));
        }
    }
    for f in m.files.iter().filter(|f| applies(&f.when, values)) {
        let contents = match f.format {
            FileFormat::Lines => {
                let mut s = expand_list(&f.lines, m, values).join("\n");
                s.push('\n');
                s
            }
            FileFormat::Text => expand_inline(f.template.as_deref().unwrap_or_default(), m, values, None),
        };
        r.files.push((f.name.clone(), contents));
        if let Some(var) = &f.env {
            early.push(format!("Set-Item -Path {} -Value (Join-Path {} {})", quote(&format!("Env:{var}")), ctx.dir, quote(&f.name)));
        }
    }
    if !early.is_empty() {
        r.early = format!("# {}\n{}\n", ctx.label, early.join("\n"));
    }

    // --- main: aliases and [powershell].init, guarded on required commands ---
    r.load = load_slot(m);
    let feature_q = quote(&ctx.feature);
    let commands: Vec<&str> = m
        .packages
        .iter()
        .filter(|p| p.applies_to_shell("powershell"))
        .flat_map(|p| p.bin.iter().map(|b| command_name(b)))
        .chain(m.requires.commands.iter().map(String::as_str))
        .collect();

    let mut body = Vec::new();
    for a in m.aliases.iter().filter(|a| applies(&a.when, values)) {
        let expanded = expand_list(&a.args, m, values);
        for line in render_alias_fn(&a.name, &a.command, &expanded).lines() {
            body.push(line.to_owned());
        }
    }
    if let Some(init) = m.powershell.init.as_ref().filter(|_| applies(&m.powershell.when, values)) {
        let pairs: Vec<String> = m
            .settings
            .iter()
            .map(|s| format!("{} = {}", quote(&s.id), quote(&values.get(&s.id).map(plain_string).unwrap_or_default())))
            .collect();
        body.push(format!("$goatty_settings = @{{ {} }}", pairs.join("; ")));
        body.push(format!("$goatty_plugin_dir = {}", ctx.dir));
        body.push(format!(". (Join-Path $goatty_plugin_dir {})", quote(init)));
        body.push("Remove-Variable -Name goatty_settings, goatty_plugin_dir -ErrorAction SilentlyContinue".into());
        r.uses_init = Some(init.clone());
    }
    if r.load == Load::Prompt {
        body.push(format!("$_goatty_prompt = {}", quote(&m.id)));
    }
    let detail = match commands.first() {
        Some(cmd) => format!("(Get-Command -Name {} -ErrorAction SilentlyContinue | Select-Object -First 1).Source", quote(cmd)),
        None => quote(&m.version),
    };
    // Only fill default ok status if init didn't already set a custom detail.
    body.push(format!("if (-not $_goatty_state.Contains({feature_q})) {{ $_goatty_state[{feature_q}] = \"ok`t\" + ({detail}) }}"));

    let mut main = format!("# --- {} ---\n", ctx.label);
    if commands.is_empty() {
        main.push_str(&body.join("\n"));
        main.push('\n');
    } else {
        let tests: Vec<String> =
            commands.iter().map(|c| format!("(Get-Command -Name {} -ErrorAction SilentlyContinue)", quote(c))).collect();
        main.push_str(&format!("if ({}) {{\n", tests.join(" -and ")));
        for line in &body {
            main.push_str(&format!("  {line}\n"));
        }
        main.push_str(&format!("}} else {{\n  $_goatty_state[{feature_q}] = 'missing'\n}}\n"));
    }
    r.main = main;
    r
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::manifest::{parse, tests::EXAMPLE};
    use crate::schema::resolve;

    fn ctx() -> Ctx<'static> {
        Ctx {
            dir: "(Join-Path $_goatty_dir 'plugins/eza')".into(),
            home: Some(r"C:\Users\u"),
            label: "eza 1.0.0".into(),
            feature: crate::zsh::feature("eza"),
        }
    }

    #[test]
    fn quotes_powershell_safely() {
        assert_eq!(quote("plain"), "'plain'");
        assert_eq!(quote("it's"), "'it''s'");
        assert_eq!(quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
        assert_eq!(quote("a; b | c"), "'a; b | c'");
        assert_eq!(quote("a\u{2019}b"), "('a' + [char]0x2019 + 'b')");
        assert_eq!(quote("a\nb"), "('a' + [char]0x000a + 'b')");
        assert_eq!(word("--icons=auto"), "--icons=auto");
        assert_eq!(word("a b"), "'a b'");
        assert_eq!(word("$env:PATH"), "'$env:PATH'");
        assert_eq!(word("a,b"), "'a,b'");
        assert_eq!(command_word("git"), "git");
        assert_eq!(command_word(r"C:\Program Files\tool.exe"), r"& 'C:\Program Files\tool.exe'");
    }

    #[test]
    fn renders_aliases_env_and_init() {
        let text = EXAMPLE.replace(
            "[permissions]",
            "[compat]\nos = [\"linux\", \"windows\"]\nshells = [\"zsh\", \"powershell\"]\n\n\
             [powershell]\ninit = \"pwsh/init.ps1\"\n\n\
             [permissions]\nshell-code = \"sets up the eza helper function\"",
        );
        let m = parse(&text, None).unwrap();
        let (v, _) = resolve(&m, &BTreeMap::new());
        let r = render(&m, &v, &ctx());
        assert_eq!(r.early, "# eza 1.0.0\nSet-Item -Path 'Env:EZA_ICON_SPACING' -Value '2'\n");
        assert!(r.main.contains("if ((Get-Command -Name 'eza' -ErrorAction SilentlyContinue)) {"), "{}", r.main);
        assert!(r.main.contains("Remove-Item -LiteralPath 'Alias:ls'"), "{}", r.main);
        assert!(r.main.contains("function global:ls { & (Get-Command -Name 'eza' -CommandType Application"), "{}", r.main);
        assert!(r.main.contains("--icons=auto @args"), "{}", r.main);
        assert!(r.main.contains(". (Join-Path $goatty_plugin_dir 'pwsh/init.ps1')"), "{}", r.main);
        assert!(r.main.contains("$_goatty_state['plugin:eza'] = 'missing'"), "{}", r.main);
        assert_eq!(r.uses_init.as_deref(), Some("pwsh/init.ps1"));
    }

    #[test]
    fn renders_special_alias_names_with_set_item() {
        let s = render_alias_fn("docker:ps", "docker", &["ps".into()]);
        assert!(s.contains("Set-Item -LiteralPath 'Function:global:docker:ps'"), "{s}");
        assert!(s.contains("Remove-Item -LiteralPath 'Alias:docker:ps'"), "{s}");
    }
}

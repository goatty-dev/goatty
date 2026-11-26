//! `goattyd snippet …`: list, show, add, remove, export and import the
//! snippets and expansions on this host. Same data as the settings
//! window's Snippets page: the user's own are `snippets` and `expansions`
//! in `host.json`; the plugins' are listed, not changed. Every change is
//! applied like the window's Apply: a validated new generation, then the
//! save.

use anyhow::{Context, Result, bail};
use goatty_plugin::snippets::{self, Owner, Pack, Snippet, SnippetInput};

use crate::service::{ApplyOutcome, HostService};

fn print_outcome(o: &ApplyOutcome) {
    match o.generation {
        Some(g) if !o.diff.is_empty() => {
            println!("Applied (generation {g}). Running shells: `goattyd shell reload`, or `goatty-reload` in this one.")
        }
        _ => println!("Saved."),
    }
    for i in &o.issues {
        println!("\u{26a0} {i}");
    }
}

fn list_of(svc: &HostService) -> Result<goatty_plugin::api::SnippetList> {
    let dirs = svc.launcher.dirs().context("$HOME is not set")?;
    Ok(crate::snippets::list(dirs, &svc.settings().value))
}

/// `goattyd snippet list`: yours, then each plugin's, then the expansions.
pub fn list(svc: &HostService) -> Result<()> {
    let l = list_of(svc)?;
    let mut owner = None;
    for s in &l.snippets {
        if owner != Some(&s.origin.owner) {
            println!(
                "{}{}",
                if owner.is_some() { "\n" } else { "" },
                if s.origin.mine() { "Yours".to_string() } else { format!("{} (plugin {})", s.origin.name, s.origin.owner) }
            );
            owner = Some(&s.origin.owner);
        }
        let missing = if s.missing.is_empty() { String::new() } else { format!("   (needs {})", s.missing.join(", ")) };
        println!("  {:<24} {}{missing}", s.snippet.id, s.snippet.title);
        println!("  {:<24} {}", "", s.snippet.line);
    }
    if l.snippets.is_empty() {
        println!("No snippets yet. Add one: goattyd snippet add \"Find big files\" 'du -ah {{dir}} | sort -rh | head'");
    }
    if !l.expansions.is_empty() {
        println!("\nExpansions{}", if l.expansions_on { "" } else { " (off: shell.expansions)" });
        for e in &l.expansions {
            let off = if e.enabled { "" } else { "   (off)" };
            println!("  {:<28} → {}   [{}]{off}", e.expansion.say, e.expansion.to, e.origin.name);
        }
    }
    for i in &l.issues {
        println!("\u{26a0} {i}");
    }
    Ok(())
}

/// `goattyd snippet show ID [--plugin P]`: everything about one snippet.
pub fn show(svc: &HostService, id: &str, plugin: Option<&str>) -> Result<()> {
    let l = list_of(svc)?;
    let s = l
        .snippets
        .iter()
        .find(|s| s.snippet.id == id && plugin.is_none_or(|p| s.origin.owner == p))
        .with_context(|| format!("no snippet {id:?} here (goattyd snippet list)"))?;
    let sn = &s.snippet;
    println!("{}  [{}]", sn.title, sn.safety().label());
    if !sn.description.is_empty() {
        println!("{}", sn.description);
    }
    println!("\n  {}\n", sn.line);
    for i in &sn.inputs {
        let mut what = vec![i.kind.label().to_string()];
        if i.optional {
            what.push("optional".into());
        }
        if let Some(d) = &i.default {
            what.push(format!("default {d}"));
        }
        if let Some(src) = &i.source {
            what.push(format!("suggestions: {}", src.describe()));
        }
        println!("  {{{}}}  {} ({})", i.id, i.label, what.join(", "));
    }
    println!("\nFrom: {}{}", s.origin.name, if sn.tags.is_empty() { String::new() } else { format!(" · tags: {}", sn.tags.join(", ")) });
    Ok(())
}

/// `goattyd snippet add TITLE LINE [--description D] [--tag T]…`: every
/// `{name}` in LINE becomes a text input.
pub fn add(svc: &HostService, title: &str, line: &str, description: Option<&str>, tags: &[String]) -> Result<()> {
    let settings = svc.settings().value;
    let id = snippets::id_from(title, &|c| settings.snippets.contains_key(c));
    let mut names: Vec<String> = Vec::new();
    for p in snippets::parse_line(line, |_| true).map_err(anyhow::Error::msg)? {
        if let snippets::Piece::Var { name, .. } = p
            && !names.contains(&name)
        {
            names.push(name);
        }
    }
    let s = Snippet {
        id: id.clone(),
        title: title.trim().to_owned(),
        description: description.unwrap_or_default().trim().to_owned(),
        tags: tags.to_vec(),
        keywords: Vec::new(),
        safety: None,
        line: line.trim().to_owned(),
        inputs: names
            .iter()
            .map(|n| {
                let label = n.replace('_', " ");
                let mut c = label.chars();
                let label = c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default();
                SnippetInput::text(n, &label)
            })
            .collect(),
        os: Vec::new(),
        shells: Vec::new(),
    };
    if let Some(p) = snippets::check_snippet(&s, &format!("snippets.{id}"), Owner::User).first() {
        bail!("{p}");
    }
    let value = s.to_user();
    print_outcome(&svc.edit(move |h| {
        h.snippets.insert(id.clone(), value);
    })?);
    println!("Added snippet {} ({} blanks). Change the questions in Settings → Snippets.", s.id, s.inputs.len());
    Ok(())
}

/// `goattyd snippet remove ID…` (yours; expansions by id too).
pub fn remove(svc: &HostService, ids: &[String]) -> Result<()> {
    let settings = svc.settings().value;
    for id in ids {
        if !settings.snippets.contains_key(id) && !settings.expansions.contains_key(id) {
            bail!("you have no snippet or expansion {id:?} (goattyd snippet list)");
        }
    }
    let ids = ids.to_vec();
    print_outcome(&svc.edit(move |h| {
        for id in &ids {
            h.snippets.remove(id);
            h.expansions.remove(id);
        }
    })?);
    Ok(())
}

/// `goattyd snippet export [FILE]`: yours as TOML (stdout without a file).
pub fn export(svc: &HostService, file: Option<&std::path::Path>) -> Result<()> {
    let settings = svc.settings().value;
    let (mine, _) = crate::snippets::user_snippets(&settings);
    let (exp, _) = crate::snippets::user_expansions(&settings);
    let pack = Pack {
        snippets: mine
            .into_iter()
            .map(|mut s| {
                // The id is the file's: it comes back on import.
                if s.id.is_empty() {
                    s.id = "snippet".into();
                }
                s
            })
            .collect(),
        expansions: exp.into_iter().map(|(_, e, _)| e).collect(),
    };
    let text = format!("# goatty snippets and expansions (goattyd snippet import FILE)\n{}", pack.to_toml());
    match file {
        Some(f) => {
            std::fs::write(f, text).with_context(|| format!("write {}", f.display()))?;
            println!("Wrote {} snippets and {} expansions to {}", pack.snippets.len(), pack.expansions.len(), f.display());
        }
        None => print!("{text}"),
    }
    Ok(())
}

/// `goattyd snippet import FILE [--force]`: add the file's snippets and
/// expansions to yours (`--force` replaces ones with the same id).
pub fn import(svc: &HostService, file: &std::path::Path, force: bool) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let pack = Pack::from_toml(&text).map_err(|e| anyhow::anyhow!("{}: {e}", file.display()))?;
    let settings = svc.settings().value;
    let mut snippets_in = Vec::new();
    let mut skipped = Vec::new();
    for mut s in pack.snippets {
        if s.id.is_empty() {
            s.id = snippets::id_from(&s.title, &|c| settings.snippets.contains_key(c));
        }
        if settings.snippets.contains_key(&s.id) && !force {
            skipped.push(s.id.clone());
            continue;
        }
        if let Some(p) = snippets::check_snippet(&s, &format!("snippet {}", s.id), Owner::User).first() {
            bail!("{p}");
        }
        snippets_in.push((s.id.clone(), s.to_user()));
    }
    let mut expansions_in = Vec::new();
    for e in pack.expansions {
        snippets::compile(&e).map_err(|why| anyhow::anyhow!("expansion {:?}: {why}", e.say))?;
        let id = snippets::id_from(e.say.trim_start_matches(['|', ' ']), &|c| settings.expansions.contains_key(c));
        if settings.expansions.values().any(|v| v.get("say").and_then(|s| s.as_str()) == Some(e.say.as_str())) && !force {
            skipped.push(e.say.clone());
            continue;
        }
        expansions_in.push((id, serde_json::to_value(&e)?));
    }
    let (n, m) = (snippets_in.len(), expansions_in.len());
    print_outcome(&svc.edit(move |h| {
        h.snippets.extend(snippets_in);
        h.expansions.extend(expansions_in);
    })?);
    println!("Imported {n} snippets and {m} expansions.");
    if !skipped.is_empty() {
        println!("Skipped (you have them already; --force replaces): {}", skipped.join(", "));
    }
    Ok(())
}

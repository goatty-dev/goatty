//! Snippets (#89): find a command by what it does, fill in its blanks,
//! and get the finished line on your prompt to read and run.
//!
//! * [`Store`]: the active host's snippets (`goatty_plugin::api::Request::
//!   Snippets`: the user's own and the enabled plugins'), fetched off the
//!   UI thread when the host changes, after plugin changes and after the
//!   user edits their own,
//! * [`picker`]: the Ctrl+Shift+S overlay (search, filters, preview),
//! * [`ask_for`]: the fill-in steps, in the command palette's input mode
//!   (`command_palette::input`), with the finished line above the fields
//!   and suggestions from the host (`Request::Suggest`),
//! * [`insert`]: the line goes onto the prompt of the focused pane (never
//!   run), through `crate::typing` (refuse and offer to copy when another
//!   program runs there or the tab is on another host).

pub mod picker;

use std::path::PathBuf;
use std::rc::Rc;

use goatty_plugin::api::{HostSnippet, Reply, Request, SnippetList};
use goatty_plugin::snippets::{Answers, Safety, Seg};
use gpui::{App, AsyncApp, Global, SharedString, Window};

use crate::command_palette::input::{Badge, Loaded, Suggestion, Tone};
use crate::command_palette::{Ask, Field};
use crate::settings_window::{HostLink, HostLinkGlobal};

/// What the store has for its host.
#[derive(Clone, Default)]
pub enum State {
    #[default]
    Idle,
    Loading,
    Ready(Rc<SnippetList>),
    Failed(SharedString),
}

/// The active host's snippets.
#[derive(Default)]
pub struct Store {
    pub link: Option<HostLink>,
    pub state: State,
    generation: u64,
}

impl Global for Store {}

/// Keep the store in step with the active host.
pub fn init(cx: &mut App) {
    cx.observe_global::<HostLinkGlobal>(|cx| {
        let link = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone());
        let same = match (&cx.default_global::<Store>().link, &link) {
            (Some(a), Some(b)) => a.same(b),
            (None, None) => true,
            _ => false,
        };
        if !same {
            refresh(cx);
        }
    })
    .detach();
}

/// Fetch the active host's snippets again (plugins or the user's own changed).
pub fn refresh(cx: &mut App) {
    let link = cx.try_global::<HostLinkGlobal>().and_then(|g| g.0.clone());
    fetch(link, cx);
}

/// Fetch `link`'s snippets unless the store has them (or is getting them).
pub fn ensure(link: &HostLink, cx: &mut App) {
    let store = cx.default_global::<Store>();
    let have = store.link.as_ref().is_some_and(|l| l.same(link)) && !matches!(store.state, State::Idle | State::Failed(_));
    if !have {
        fetch(Some(link.clone()), cx);
    }
}

fn fetch(link: Option<HostLink>, cx: &mut App) {
    let store = cx.default_global::<Store>();
    store.generation += 1;
    let generation = store.generation;
    store.link = link.clone();
    store.state = if link.is_some() { State::Loading } else { State::Idle };
    let Some(link) = link else { return };
    cx.spawn(async move |cx: &mut AsyncApp| {
        let l = link.clone();
        let reply = cx.background_executor().spawn(async move { l.plugin(Request::Snippets) }).await;
        cx.update(|cx| {
            let store = cx.default_global::<Store>();
            if store.generation != generation {
                return;
            }
            store.state = match reply {
                Ok(Reply::Snippets(list)) => State::Ready(Rc::new(list)),
                Ok(_) => State::Failed("unexpected reply from the host".into()),
                Err(e) => {
                    log::info!("snippets: couldn't list {}'s: {e:#}", link.label());
                    let first = format!("{e:#}").lines().next().unwrap_or_default().to_string();
                    let msg = if first.contains("unknown plugin request") {
                        format!("goattyd on {} is older than this goatty and has no snippets yet: update it", link.label())
                    } else {
                        format!("Couldn't list the snippets on {}: {first}", link.label())
                    };
                    State::Failed(msg.into())
                }
            };
        });
    })
    .detach();
}

/// The badge for a snippet's safety.
pub fn safety_badge(s: Safety) -> Badge {
    Badge {
        text: s.label().into(),
        tone: match s {
            Safety::Safe => Tone::Good,
            Safety::Changes => Tone::Warn,
            Safety::Destructive => Tone::Danger,
        },
        tooltip: s.meaning().into(),
    }
}

/// The recently-used id of a snippet on a host.
pub fn history_id(link: &HostLink, s: &HostSnippet) -> String {
    format!("{}/{}.{}", link.remote_key().unwrap_or_else(|| "local".into()), s.origin.owner, s.snippet.id)
}

/// The fill-in steps for `hs` on `link`: one field per input, the finished
/// line above them, then [`insert`]. `cwd`: the pane's directory, where
/// sources run. `back`: Esc on the first field (the picker again).
pub fn ask_for(hs: &HostSnippet, link: &HostLink, cwd: Option<PathBuf>, back: Option<crate::command_palette::input::WindowAction>) -> Ask {
    let snippet = Rc::new(hs.snippet.clone());
    let fields: Vec<Field> = snippet
        .inputs
        .iter()
        .map(|input| {
            let loader = input.source.as_ref().map(|_| {
                let (link, owner, id, input_id, cwd) =
                    (link.clone(), hs.origin.owner.clone(), snippet.id.clone(), input.id.clone(), cwd.clone());
                let loader: crate::command_palette::input::Loader = Rc::new(move |answers: &Answers| {
                    let req = Request::Suggest {
                        owner: owner.clone(),
                        snippet: id.clone(),
                        input: input_id.clone(),
                        values: answers.clone(),
                        cwd: cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
                    };
                    let link = link.clone();
                    Box::new(move || suggestions(link.plugin(req)))
                });
                loader
            });
            Field::from_snippet(input, loader)
        })
        .collect();
    let then_snippet = snippet.clone();
    let origin = link.clone();
    let mut ask = Ask::new(snippet.title.clone(), fields, move |answers, window, cx| {
        insert(&then_snippet.render(answers), &origin, window, cx);
    })
    .remote(!link.is_local());
    let preview_snippet = snippet.clone();
    ask.preview = Some(Rc::new(move |answers: &Answers, current: &str, text: &str| -> Vec<Seg> {
        let mut values = answers.clone();
        // What's typed shows as it'll be inserted; an empty field is a blank.
        values.remove(current);
        if !text.is_empty()
            && let Some(input) = preview_snippet.input(current)
            && (input.check(text).is_ok() || input.kind == goatty_plugin::commands::InputKind::Text)
        {
            values.insert(current.to_string(), text.to_string());
        }
        preview_snippet.preview(&values, Some(current))
    }));
    ask.badge = Some(safety_badge(snippet.safety()));
    ask.back = back;
    ask.done_label = Some("put it on the prompt".into());
    ask
}

/// A `Suggest` reply as the palette's suggestions.
fn suggestions(reply: anyhow::Result<Reply>) -> Loaded {
    match reply {
        Ok(Reply::Suggestions(s)) => Loaded {
            items: s.items.into_iter().map(|i| Suggestion { label: i.value.clone(), value: i.value, detail: i.detail }).collect(),
            note: s.note,
        },
        Ok(_) => Loaded { items: Vec::new(), note: Some("Unexpected reply from the host".into()) },
        Err(e) => {
            let first = format!("{e:#}").lines().next().unwrap_or_default().to_string();
            let note = if first.contains("unknown plugin request") {
                "This host's goattyd can't suggest values yet (update it); type one".to_string()
            } else {
                format!("No suggestions: {first}")
            };
            Loaded { items: Vec::new(), note: Some(note) }
        }
    }
}

/// Put `line` on the prompt of the focused pane (deferred: called from
/// inside the workspace).
pub fn insert(line: &str, origin: &HostLink, window: &mut Window, cx: &mut App) {
    let (line, origin) = (line.to_string(), origin.clone());
    window.defer(cx, move |window, cx| {
        if let Some(view) = crate::typing::target(&origin, &line, crate::typing::What::Snippet, window, cx) {
            view.update(cx, |v, cx| v.insert_line(&line, cx));
        }
    });
}

/// "Save the last command as a snippet": the focused pane's last command
/// line (from the prompt marks) goes into a new snippet in Settings →
/// Snippets, where `{name}`s turn parts of it into blanks.
pub fn save_last(window: &mut Window, cx: &mut App) {
    window.defer(cx, |window, cx| {
        let ws = window.root::<crate::workspace::Workspace>().flatten();
        let line = ws.and_then(|ws| ws.read(cx).focused_terminal()).and_then(|v| {
            let t = v.read(cx).session().terminal.transcript();
            let b = t.last_command()?;
            t.command_line(b).map(crate::settings_window::snippets::strip_prompt).filter(|l| !l.is_empty())
        });
        match line {
            Some(line) => crate::settings_window::snippets::new_from_line(line, cx),
            None => {
                crate::toast::show(
                    cx,
                    crate::toast::Toast::info("No command to save yet")
                        .body("Run a command in this pane first (it needs goatty's shell integration to find it).")
                        .key("snippets.save_last"),
                );
            }
        }
    });
}

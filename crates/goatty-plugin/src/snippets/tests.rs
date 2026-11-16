use super::*;
use crate::manifest::{parse, tests::EXAMPLE};

fn snippet(toml_text: &str) -> Snippet {
    toml::from_str(toml_text).unwrap_or_else(|e| panic!("{e}"))
}

fn values(pairs: &[(&str, &str)]) -> Answers {
    pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
}

fn with_snippets(extra: &str) -> Result<crate::Manifest, crate::manifest::Invalid> {
    let (head, perms) = EXAMPLE.split_once("[permissions]").unwrap();
    let mut perms = perms.to_string();
    if extra.contains("[snippet.input.source]") || extra.contains("source = {") {
        perms.push_str("snippet-sources = \"lists the branches of the repository you're in\"\n");
    }
    parse(&format!("{head}{extra}\n[permissions]{perms}"), None)
}

const FIND: &str = r#"
id = "find-big"
title = "Find big files"
line = "find {dir} -type f -size +{size}M {more}"

[[input]]
id = "dir"
label = "Folder"
type = "folder"
default = "."

[[input]]
id = "size"
label = "Bigger than (MB)"
type = "integer"
min = 1

[[input]]
id = "more"
label = "Also show hidden files?"
type = "flag"
on = "-name '.*'"
"#;

#[test]
fn lexes_placeholders_quotes_and_shell_braces() {
    let known = |n: &str| ["a", "b"].contains(&n);
    let p = parse_line("echo {a} x{b}y '{print $1}' ${HOME} {} {x,y} {{a}}", known).unwrap();
    let vars: Vec<(String, bool, bool)> = p
        .iter()
        .filter_map(|p| match p {
            Piece::Var { name, start, end } => Some((name.clone(), *start, *end)),
            Piece::Text(_) => None,
        })
        .collect();
    assert_eq!(vars, [("a".into(), true, true), ("b".into(), false, false)]);
    let text: String = p.iter().map(|p| if let Piece::Text(t) = p { t.as_str() } else { "" }).collect();
    assert!(text.ends_with(" '{print $1}' ${HOME} {} {x,y} {a}"), "{text}");
    assert!(parse_line("echo '{a}'", known).unwrap_err().contains("inside quotes"));
    assert!(parse_line("echo \"x {b}\"", known).unwrap_err().contains("inside quotes"));
    assert!(parse_line("echo $'{a}'", known).unwrap_err().contains("inside quotes"));
    assert!(parse_line("echo {nope}", known).unwrap_err().contains("no input"));
    assert!(parse_line("echo 'open", known).unwrap_err().contains("quote"));
    assert!(parse_line("echo \\", known).is_err());
    assert!(parse_line("a\nb", known).is_err());
    // An escaped quote doesn't open one; a quote inside the other kind is text.
    assert!(parse_line(r#"echo \' {a} "it's" {b}"#, known).is_ok());
    // Placeholders next to each other: neither is a whole word.
    let p = parse_line("{a}{b}", known).unwrap();
    assert!(matches!(&p[0], Piece::Var { start: true, end: false, .. }));
    assert!(matches!(&p[1], Piece::Var { start: false, end: true, .. }));
    let p = parse_line("x|{a};", known).unwrap();
    assert!(matches!(&p[1], Piece::Var { start: true, end: true, .. }), "operators end words");
}

#[test]
fn renders_quoted_words() {
    let s = snippet(FIND);
    assert_eq!(s.render(&values(&[("dir", "my dir"), ("size", "100"), ("more", "no")])), "find 'my dir' -type f -size +100M");
    assert_eq!(s.render(&values(&[("dir", "~/src"), ("size", "5"), ("more", "yes")])), "find ~/src -type f -size +5M -name '.*'");
    assert_eq!(s.render(&values(&[("dir", "~"), ("size", "5"), ("more", "no")])), "find ~ -type f -size +5M");
    let s2 = Snippet { line: "tar -C{dir} x".into(), inputs: vec![s.inputs[0].clone()], ..s.clone() };
    assert_eq!(s2.render(&values(&[("dir", "~/a b")])), r#"tar -C"$HOME"/'a b' x"#, "~ inside a word");
    assert_eq!(
        s.render(&values(&[("dir", "$(rm -rf ~)"), ("size", "1"), ("more", "no")])),
        "find '$(rm -rf ~)' -type f -size +1M",
        "values never become shell code"
    );
    // Several values, each its own word.
    let add = snippet(
        r#"
id = "add"
title = "Stage files"
line = "git add -- {files}"
[[input]]
id = "files"
label = "Files"
multiple = true
source = { run = ["git", "ls-files", "-m"] }
"#,
    );
    assert_eq!(add.render(&values(&[("files", "a b\nc\n")])), "git add -- 'a b' c");
}

#[test]
fn optional_values_drop_out_with_their_space() {
    let s = snippet(
        r#"
id = "grep"
title = "Search"
line = "grep {flags} -- {text} {file} | head"
[[input]]
id = "flags"
label = "Flags"
type = "choice"
optional = true
choices = [{ value = "-i" }, { value = "-ri" }]
[[input]]
id = "text"
label = "Text"
[[input]]
id = "file"
label = "File"
type = "path"
optional = true
"#,
    );
    assert_eq!(s.render(&values(&[("flags", ""), ("text", "a b"), ("file", "")])), "grep -- 'a b' | head");
    assert_eq!(s.render(&values(&[("flags", "-ri"), ("text", "x"), ("file", "f.txt")])), "grep -ri -- x f.txt | head");
}

#[test]
fn preview_marks_blanks_and_the_current_one() {
    let s = snippet(FIND);
    let segs = s.preview(&values(&[("dir", ".")]), Some("size"));
    let shown: String = segs.iter().map(Seg::text).collect();
    assert_eq!(shown, "find . -type f -size +‹Bigger than (MB)›M ‹Also show hidden files?›");
    assert!(segs.iter().any(|s| matches!(s, Seg::Blank { id, current: true, .. } if id == "size")));
    assert!(segs.iter().any(|s| matches!(s, Seg::Value { id, text } if id == "dir" && text == ".")));
}

#[test]
fn hostile_values_survive_a_real_shell() {
    // `printf '%s\n'` in place of the program prints each value back.
    let s = snippet(
        r#"
id = "echo"
title = "Echo"
line = "printf '%s\\n' {a} pre-{b} {c}"
[[input]]
id = "a"
label = "A"
[[input]]
id = "b"
label = "B"
[[input]]
id = "c"
label = "C"
multiple = true
source = { sh = "true" }
"#,
    );
    let nasty = ["it's \"$HOME\" `id` $(echo hi); ls *", "\\ ' \" ! ~ # & {x} {{", "~/not-home"];
    let line = s.render(&values(&[("a", nasty[0]), ("b", nasty[1]), ("c", &format!("{}\n;exit 1", nasty[2]))]));
    for shell in ["sh", "zsh"] {
        let Ok(out) = std::process::Command::new(shell).arg("-c").arg(&line).output() else { continue };
        assert!(out.status.success(), "{shell}: {line}");
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("{}\npre-{}\n{}\n;exit 1\n", nasty[0], nasty[1], nasty[2]),
            "{shell}: {line}"
        );
    }
}

#[test]
fn validation() {
    let bad = r#"
[[snippet]]
id = "Bad"
title = ""
line = "rm '{a}' {b} {zzz}"
[[snippet.input]]
id = "a"
label = "A"
type = "flag"
[[snippet.input]]
id = "b"
label = "B"
strict = true
[[snippet.input]]
id = "c"
label = "C"
[snippet.input.source]
run = ["curl", "{later}"]
sh = "x"
[[snippet.input]]
id = "later"
label = "Later"
"#;
    let p: Vec<String> = with_snippets(bad).unwrap_err().0.iter().map(ToString::to_string).collect();
    let has = |s: &str| p.iter().any(|x| x.contains(s));
    assert!(has("snippet[0].id: \"Bad\""), "{p:?}");
    assert!(has("snippet[0].title: empty"), "{p:?}");
    assert!(has("input[0].on: a flag needs `on`"), "{p:?}");
    assert!(has("input[1].strict"), "{p:?}");
    assert!(has("input[2].source.sh: plugins list a program"), "{p:?}");
    assert!(has("is asked later"), "{p:?}");
    assert!(has("snippet[0].line: {a} is inside quotes"), "{p:?}");

    let unused = "[[snippet]]\nid = \"x\"\ntitle = \"X\"\nline = \"eza\"\n[[snippet.input]]\nid = \"a\"\nlabel = \"A\"\n";
    let p: Vec<String> = with_snippets(unused).unwrap_err().0.iter().map(ToString::to_string).collect();
    assert!(p.iter().any(|x| x.contains("doesn't use input \"a\"")), "{p:?}");

    let foreign = "[[snippet]]\nid = \"x\"\ntitle = \"X\"\nline = \"eza {a}\"\n[[snippet.input]]\nid = \"a\"\nlabel = \"A\"\n[snippet.input.source]\nrun = [\"rm\", \"-rf\"]\n";
    let p: Vec<String> = with_snippets(foreign).unwrap_err().0.iter().map(ToString::to_string).collect();
    assert!(p.iter().any(|x| x.contains("\"rm\" is neither")), "{p:?}");

    let glued = "[[snippet]]\nid = \"x\"\ntitle = \"X\"\nline = \"eza --x={a}\"\n[[snippet.input]]\nid = \"a\"\nlabel = \"A\"\nmultiple = true\ntype = \"choice\"\nchoices = [{ value = \"1\" }]\n";
    let p: Vec<String> = with_snippets(glued).unwrap_err().0.iter().map(ToString::to_string).collect();
    assert!(p.iter().any(|x| x.contains("word of its own")), "{p:?}");

    // An input only a later source uses is fine (it narrows the suggestions).
    let narrow = "[[snippet]]\nid = \"x\"\ntitle = \"X\"\nline = \"eza {f}\"\n[[snippet.input]]\nid = \"d\"\nlabel = \"D\"\n[[snippet.input]]\nid = \"f\"\nlabel = \"F\"\n[snippet.input.source]\nrun = [\"eza\", \"{d}\"]\n";
    let m = with_snippets(narrow).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(m.snippets[0].render(&values(&[("d", "src"), ("f", "a.rs")])), "eza a.rs");

    // The user's own may use `sh`, and any program.
    let mine = snippet(
        "id = \"mine\"\ntitle = \"Mine\"\nline = \"git switch {b}\"\n[[input]]\nid = \"b\"\nlabel = \"B\"\nsource = { sh = \"git branch | cut -c3-\" }\n",
    );
    assert_eq!(check_snippet(&mine, "snippets.mine", Owner::User), []);
}

#[test]
fn snippets_are_permissions() {
    use crate::permissions::{Permission, derived, describe};
    let ok = r#"
[[snippet]]
id = "list"
title = "List a folder"
line = "eza -l {dir}"
[[snippet.input]]
id = "dir"
label = "Folder"
type = "folder"
[snippet.input.source]
run = ["eza", "-D"]

[[expansion]]
say = "| first {n} lines"
to = "| head -n {n}"
input.n.type = "integer"
"#;
    let m = with_snippets(ok).unwrap_or_else(|e| panic!("{e}"));
    let d = derived(&m);
    assert!(d.contains(&Permission::Snippets) && d.contains(&Permission::SnippetSources));
    let g = describe(&m);
    let get = |p| g.iter().find(|g| g.permission == p).unwrap();
    assert_eq!(get(Permission::Snippets).details, ["List a folder: eza -l {dir}", "| first {n} lines → | head -n {n}"]);
    assert_eq!(get(Permission::SnippetSources).details, ["List a folder → Folder: eza -D"]);
    // Sources need a reason.
    let (head, perms) = EXAMPLE.split_once("[permissions]").unwrap();
    let p = parse(&format!("{head}{ok}\n[permissions]{perms}"), None).unwrap_err().0;
    assert!(p.iter().any(|p| p.path == "permissions.snippet-sources"), "{p:?}");
}

#[test]
fn parses_source_output() {
    let src = Source {
        run: vec!["git".into()],
        sh: None,
        split: Some("\t".into()),
        column: None,
        detail: None,
        label: None,
        skip: 0,
        cwd: SourceCwd::Pane,
        on_fail: None,
        timeout: None,
    };
    let (items, more) = parse_output("main\tFix it\nfeature/x\t\x1b[31mWIP\x1b[0m\nmain\tagain\n\n", &src);
    assert!(!more);
    assert_eq!(
        items,
        [
            Item { value: "main".into(), label: None, detail: Some("Fix it".into()) },
            Item { value: "feature/x".into(), label: None, detail: Some("WIP".into()) }
        ]
    );
    let ps = Source { split: Some(" ".into()), column: Some(2), detail: Some(1), skip: 1, ..src.clone() };
    let (items, _) = parse_output("  PID COMMAND\n  12 zsh\n 345  cargo build\n", &ps);
    assert_eq!(items[0], Item { value: "zsh".into(), label: None, detail: Some("12".into()) });
    assert_eq!(items[1], Item { value: "cargo".into(), label: None, detail: Some("345".into()) });
    // A process id to insert, its name to show and search.
    let pids = Source { split: Some(" ".into()), column: Some(1), label: Some(2), ..src.clone() };
    let (items, _) = parse_output("  12 zsh\n", &pids);
    assert_eq!(items[0], Item { value: "12".into(), label: Some("zsh".into()), detail: None });
    let plain = Source { split: None, ..src.clone() };
    let (items, _) = parse_output(" a b \n", &plain);
    assert_eq!(items, [Item { value: "a b".into(), label: None, detail: None }]);
    let many: String = (0..MAX_ITEMS + 5).map(|i| format!("{i}\n")).collect();
    let (items, more) = parse_output(&many, &plain);
    assert!(more && items.len() == MAX_ITEMS);
}

#[test]
fn source_commands_take_earlier_values_as_arguments() {
    let s = snippet(
        r#"
id = "commit"
title = "Show a commit"
line = "git show {commit}"
[[input]]
id = "branch"
label = "Branch"
[[input]]
id = "commit"
label = "Commit"
source = { run = ["git", "log", "--format=%h\t%s", "{branch}", "--max-count={n}"] }
"#,
    );
    let input = &s.inputs[1];
    assert_eq!(input.depends(), ["branch", "n"]);
    assert_eq!(source_command(&s, input, &Answers::new()).unwrap_err(), "Fill in Branch first");
    let cmd = source_command(&s, input, &values(&[("branch", "x; rm -rf ~"), ("n", "5")])).unwrap();
    assert_eq!(
        cmd,
        Command::Argv(vec!["git".into(), "log".into(), "--format=%h\t%s".into(), "x; rm -rf ~".into(), "--max-count=5".into()])
    );
}

#[test]
fn finds_programs_and_guesses_safety() {
    assert_eq!(programs("sudo -E apt install {pkg} && FOO=1 env x=2 make | less"), ["sudo", "apt", "make", "less"]);
    assert_eq!(programs("echo $(date) `id` 'ls'"), ["echo", "date", "id"]);
    let g = guess_safety;
    assert_eq!(g("ls -la"), Safety::Safe);
    assert_eq!(g("du -sh * | sort -h"), Safety::Safe);
    assert_eq!(g("grep x f > /dev/null 2>&1"), Safety::Safe);
    assert_eq!(g("echo hi > notes.txt"), Safety::Changes);
    assert_eq!(g("tar -xzf {archive}"), Safety::Changes);
    assert_eq!(g("tar -tzf {archive}"), Safety::Safe);
    assert_eq!(g("git switch {branch}"), Safety::Changes);
    assert_eq!(g("git status"), Safety::Safe);
    assert_eq!(g("git reset --hard HEAD~1"), Safety::Destructive);
    assert_eq!(g("find . -name '*.tmp' -delete"), Safety::Destructive);
    assert_eq!(g("sudo rm -rf {dir}"), Safety::Destructive);
    assert_eq!(g("sudo systemctl status nginx"), Safety::Changes, "sudo always counts");
    let s = snippet(FIND);
    assert_eq!(s.safety(), Safety::Safe);
    assert_eq!(Snippet { safety: Some(Safety::Destructive), ..s }.safety(), Safety::Destructive);
}

#[test]
fn user_snippets_round_trip_without_their_id() {
    let s = snippet(FIND);
    let v = s.to_user();
    assert!(v.get("id").is_none());
    assert_eq!(Snippet::from_user("find-big", &v).unwrap(), s);
    assert!(Snippet::from_user("x", &serde_json::json!({"title": "T", "line": "ls", "oops": 1})).is_err());
}

#[test]
fn expansions_compile_to_matching_rules() {
    let e = |say: &str, to: &str| Expansion { say: say.into(), to: to.into(), description: String::new(), inputs: BTreeMap::new() };
    let mut first = e("| first {n} lines", "| head -n {n}");
    first.inputs.insert("n".into(), ExpansionInput { kind: ExpansionKind::Integer });
    let c = compile(&first).unwrap();
    assert_eq!(c.last, "lines");
    assert_eq!(c.re, r"(^|[[:space:]])\|[[:space:]]+first[[:space:]]+([0-9]+)[[:space:]]+lines$");
    assert_eq!(c.apply("cat log | first 200 lines").as_deref(), Some("cat log | head -n 200"));
    assert_eq!(c.apply("cat log |  first   20 lines").as_deref(), Some("cat log | head -n 20"));
    assert_eq!(c.apply("cat log | first many lines"), None, "integers only");
    assert_eq!(c.apply("cat log | first 2 lines now"), None, "only at the end");
    assert_eq!(c.apply("x|first 2 lines"), None, "whole words");

    let grep = compile(&e("| only lines with {text}", "| grep -- {text}")).unwrap();
    assert_eq!((grep.last.as_str(), grep.trailing), ("with", true));
    assert_eq!(grep.apply("dmesg | only lines with 'usb error'"), None, "a word has no spaces");
    assert_eq!(grep.apply("dmesg | only lines with usb").as_deref(), Some("dmesg | grep -- usb"));
    // Literals that mean something in a regular expression.
    let dots = compile(&e("count (a.b) [x]*", "wc")).unwrap();
    assert_eq!(dots.apply("count (a.b) [x]*").as_deref(), Some("wc"));
    assert_eq!(dots.apply("count (aXb) [x]*"), None);

    for (bad, why) in [
        (e("sorted", "sort"), "two words"),
        (e("| first {n} {m}", "head -n {n}"), "at most one placeholder"),
        (e("| x {a}{b} y", "z"), "word of its own"),
        (e("| it's x", "z"), "no quotes"),
        (e("| x y", "{nope}"), "isn't in `say`"),
        (e("| x y", "a\u{7}"), "control"),
    ] {
        let err = compile(&bad).unwrap_err();
        assert!(err.contains(why), "{bad:?}: {err}");
    }
}

#[test]
fn the_table_is_quoted_data() {
    let e =
        |say: &str, to: &str| compile(&Expansion { say: say.into(), to: to.into(), description: String::new(), inputs: BTreeMap::new() });
    let rules = vec![e("| sorted nicely", "| sort").unwrap(), e("| x {w} nicely", "| y '$(rm -rf ~)' {w}").unwrap()];
    let t = render_table(&rules);
    assert!(t.contains("typeset -gA _goatty_exp_last=('nicely' '1 2')"), "{t}");
    assert!(t.contains(r"$'| y \'$(rm -rf ~)\' \x012\x01'"), "{t}");
    // zsh reads it back as the same data.
    let Ok(out) = std::process::Command::new("zsh")
        .arg("-fc")
        .arg(format!("{t}\nprint -rl -- $_goatty_exp_re[2] ${{(q+)_goatty_exp_to[2]}} $_goatty_exp_last[nicely]"))
        .output()
    else {
        return;
    };
    let out = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out, format!("{}\n$'| y \\'$(rm -rf ~)\\' \\C-A2\\C-A'\n1 2\n", rules[1].re), "zsh's (q+) shows \\x01 as \\C-A");
}

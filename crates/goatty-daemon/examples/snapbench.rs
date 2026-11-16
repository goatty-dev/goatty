//! What compression saves on snapshots and history, and what it and
//! inserting fetched history cost (numbers in `docs/remote-architecture.md` §7).
//!
//! `cargo run --release -p goatty-daemon --example snapbench [FILE]`
//!
//! Feeds FILE (default: this repository's Rust sources, colored like a
//! syntax highlighter would) into a 120×40 terminal, then measures the
//! remote attach snapshot (2000 lines), a 1000-line history fetch and
//! inserting it into a client terminal.

use std::sync::Arc;
use std::time::Instant;

use goatty_proto::{ServerMsg, encode, encode_server};
use goatty_term::{Palette, TermSize, Terminal, TopLineCause};

fn sources() -> Vec<u8> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    let mut dirs = vec![root.join("crates")];
    while let Some(d) = dirs.pop() {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() && !p.ends_with("target") {
                dirs.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut out = Vec::new();
    for f in files {
        for line in std::fs::read_to_string(f).unwrap().lines() {
            // A little color, like `bat` or a compiler would print.
            let colored = line.replacen("fn ", "\x1b[34mfn\x1b[0m ", 1).replacen("let ", "\x1b[35mlet\x1b[0m ", 1);
            out.extend_from_slice(colored.as_bytes());
            out.extend_from_slice(b"\r\n");
        }
    }
    out
}

fn time<R>(what: &str, runs: u32, mut f: impl FnMut() -> R) -> R {
    let start = Instant::now();
    let mut r = f();
    for _ in 1..runs {
        r = f();
    }
    println!("{what:<44} {:>8.2} ms", start.elapsed().as_secs_f64() * 1000.0 / runs as f64);
    r
}

fn main() {
    let input = match std::env::args().nth(1) {
        Some(path) => std::fs::read(path).unwrap(),
        None => sources(),
    };
    let size = TermSize::new(120, 40);
    let (daemon, _c) = Terminal::new(size, true, Arc::new(Palette::default()));
    daemon.feed(&input);
    println!("history: {} lines", daemon.history_size());

    let snapshot = daemon.reattach_snapshot_with_history(2_000);
    let attached = ServerMsg::Attached { req: 1, session: 1, snapshot: snapshot.clone() };
    let plain = encode(&attached).unwrap();
    let packed = time("attach snapshot (2000 lines), zstd", 20, || encode_server(&attached, true).unwrap());
    println!("  {} KB → {} KB ({:.1}×)", plain.len() / 1024, packed.len() / 1024, plain.len() as f64 / packed.len() as f64);
    let read = goatty_proto::read_msg::<_, ServerMsg>(&mut &packed[..]).unwrap();
    time("  decompress", 20, || read.clone().unpack().unwrap());

    let top = daemon.top_line().unwrap();
    let before = top - 2_000;
    let rows = time("history rows (1000 lines)", 20, || daemon.history_rows(before, 1_000));
    let history = ServerMsg::History { req: 1, session: 1, before, first: rows.first, oldest: rows.oldest, data: rows.data.clone() };
    let plain = encode(&history).unwrap();
    let packed = time("  zstd", 20, || encode_server(&history, true).unwrap());
    println!("  {} KB → {} KB ({:.1}×)", plain.len() / 1024, packed.len() / 1024, plain.len() as f64 / packed.len() as f64);

    // A client with the attach snapshot inserts the fetch above it, then
    // with a full scrollback (the worst case: the whole grid is rebuilt).
    for history_lines in [2_000, 8_000] {
        let (client, _c) = Terminal::new(size, false, Arc::new(Palette::default()));
        client.feed(&daemon.reattach_snapshot_with_history(history_lines));
        client.set_top_line(top, TopLineCause::Attach);
        let before = top - client.history_size() as u64;
        let rows = daemon.history_rows(before, 1_000);
        let start = Instant::now();
        assert!(client.insert_history(before, &rows));
        println!("{:<44} {:>8.2} ms", format!("insert 1000 lines above {history_lines}"), start.elapsed().as_secs_f64() * 1000.0);
    }
}

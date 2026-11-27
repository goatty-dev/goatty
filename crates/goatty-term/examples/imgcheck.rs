//! Runs a command on a PTY behind a daemon-like terminal (media rewriting,
//! query answers) and a mirror fed the same bytes, like goattyd and a
//! client. Prints both screens and their images, and whether they agree.
//!
//! cargo run --release -p goatty-term --example imgcheck -- SECONDS CMD [ARGS...]
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};

use goatty_term::pty::{Pty, ShellCommand};
use goatty_term::{Frame, MediaRewriter, Palette, TermSize, Terminal};

fn main() {
    let mut args = std::env::args().skip(1);
    let secs: f64 = args.next().expect("seconds").parse().unwrap();
    let program = args.next().expect("command");
    let size = TermSize { cols: 100, rows: 30, cell_width: 10, cell_height: 20 };
    let env = vec![("TERM".into(), "xterm-256color".into())];
    let (pty, mut reader) = Pty::spawn(&ShellCommand::new(program, args.collect(), env), size).unwrap();
    let mut input = pty.input().unwrap();
    let (daemon, ch) = Terminal::new(size, true, Arc::new(Palette::default()));
    let (mirror, _mch) = Terminal::new(size, false, Arc::new(Palette::default()));
    let (tx, rx) = flume::unbounded::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut media = MediaRewriter::new();
    let (mut bytes, mut fed, mut rewrite, mut mirror_t, mut raw) = (0usize, Duration::ZERO, Duration::ZERO, Duration::ZERO, 0usize);
    let mut frames = 0usize;
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(20)) {
            raw += chunk.len();
            frames += memchr::memmem::find_iter(&chunk, b"a=T").count();
            let t0 = Instant::now();
            let chunk = media.rewrite(&chunk);
            let t1 = Instant::now();
            daemon.feed(&chunk);
            let t2 = Instant::now();
            mirror.feed(&chunk);
            rewrite += t1 - t0;
            fed += t2 - t1;
            mirror_t += t2.elapsed();
            bytes += chunk.len();
        }
        for reply in ch.replies.try_iter() {
            let _ = input.write(&reply);
        }
    }
    eprintln!("{raw} bytes read, {bytes} fed; rewriting {rewrite:?}, daemon {fed:?}, mirror {mirror_t:?}");
    if let Some(each) = bytes.checked_div(frames) {
        let per = |d: Duration| d / frames as u32;
        eprintln!(
            "{frames} images (a=T): {} KiB each as fed; per image: rewriting {:?}, daemon {:?}, mirror {:?}",
            each / 1024,
            per(rewrite),
            per(fed),
            per(mirror_t)
        );
    }
    let (mut a, mut b) = (Frame::default(), Frame::default());
    daemon.snapshot_into(&mut a);
    mirror.snapshot_into(&mut b);
    dump("daemon", &a);
    let same = text(&a) == text(&b) && images(&a) == images(&b);
    eprintln!("mirror matches daemon: {same}");
    if !same {
        dump("mirror", &b);
    }
    let (c, _cch) = Terminal::new(size, false, Arc::new(Palette::default()));
    c.feed(&daemon.reattach_snapshot());
    let mut f = Frame::default();
    c.snapshot_into(&mut f);
    eprintln!("attached client matches: {}", text(&f) == text(&a) && images(&f) == images(&a));
}

fn text(f: &Frame) -> Vec<String> {
    (0..f.rows).map(|r| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string()).collect()
}

fn images(f: &Frame) -> Vec<String> {
    f.images
        .iter()
        .map(|i| {
            // The pixels too (by hash: ids differ between terminals).
            let mut h = std::hash::DefaultHasher::new();
            std::hash::Hash::hash(&i.image.rgba, &mut h);
            format!(
                "{}x{} #{:016x} at ({:.2},{:.2}) size {:.2}x{:.2} src {:?} {:?}",
                i.image.width,
                i.image.height,
                std::hash::Hasher::finish(&h),
                i.x,
                i.y,
                i.width,
                i.height,
                i.source,
                i.layer
            )
        })
        .collect()
}

fn dump(name: &str, f: &Frame) {
    eprintln!("--- {name}: cursor {:?}", f.cursor.map(|c| (c.col, c.row)));
    let rows = text(f);
    let last = rows.iter().rposition(|r| !r.is_empty()).map_or(0, |l| l + 1);
    for (i, r) in rows[..last].iter().enumerate() {
        eprintln!("{i:2}|{r}");
    }
    for i in images(f) {
        eprintln!("  image {i}");
    }
}

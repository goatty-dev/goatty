//! Kitty graphics sent through a file, a temporary file or shared memory
//! (`t=f`, `t=t`, `t=s`), rewritten into direct transmissions (`t=d`).
//!
//! A session's output is parsed by more than one terminal: the daemon's,
//! which answers queries, and a mirror in every client, maybe on another
//! computer. Only the first can read the program's file, and for `t=t` and
//! `t=s` it deletes it after reading (the protocol says so). So whoever
//! reads the PTY rewrites these into `t=d` with the data inline before any
//! terminal sees them, and every terminal decodes the same bytes.
//!
//! Everything else passes through untouched and unbuffered, except a few
//! bytes of an escape sequence split across reads. What may be read follows
//! kitty's rules (the program could read those files itself): regular files
//! only, not under /proc, /sys or /dev (except /dev/shm), and a temporary
//! file must be in a temporary directory and have `tty-graphics-protocol`
//! in its name. When reading fails the command passes through unchanged and
//! the terminal reports the error.

use std::borrow::Cow;
use std::path::Path;

use crate::graphics::base64_into;

/// Largest payload read from a medium (rio-vt's limit for an image).
const MAX_DATA: u64 = 400 * 1024 * 1024;
/// A header (the keys before `;`) longer than this isn't one we rewrite.
const MAX_HEADER: usize = 1024;
/// A file name or shared memory name longer than this (base64) isn't one.
const MAX_NAME: usize = 8 * 1024;

#[derive(Default)]
pub struct MediaRewriter {
    state: State,
    /// Bytes of an escape sequence held back until it's clear whether it's
    /// one to rewrite: `ESC`, `ESC _`, `ESC _ G keys`, or the header and
    /// payload of a command being rewritten.
    held: Vec<u8>,
    /// Where the payload starts in `held` (after `;`), while capturing.
    payload_at: usize,
}

#[derive(Default, Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    #[default]
    Ground,
    /// After ESC.
    Esc,
    /// After `ESC _`.
    Apc,
    /// After `ESC _ G`: reading the keys.
    Header,
    /// A kitty command that stays as it is: copying up to its end.
    Pass,
    /// A command to rewrite: collecting its payload (the name).
    Capture,
}

impl MediaRewriter {
    pub fn new() -> Self {
        Self::default()
    }

    /// The bytes to feed and forward in place of `input`. Usually `input`
    /// itself; bytes of a sequence that continues in the next read come out
    /// with that read.
    pub fn rewrite<'a>(&mut self, input: &'a [u8]) -> Cow<'a, [u8]> {
        let clean = match self.state {
            State::Ground => memchr::memmem::find(input, b"\x1b_G").is_none() && !input.ends_with(b"\x1b") && !input.ends_with(b"\x1b_"),
            State::Pass => apc_end(input).is_none(),
            _ => false,
        };
        if clean {
            return Cow::Borrowed(input);
        }
        let mut out = Vec::with_capacity(input.len() + self.held.len());
        let mut i = 0;
        while i < input.len() {
            match self.state {
                State::Ground => match memchr::memchr(0x1b, &input[i..]) {
                    Some(n) => {
                        out.extend_from_slice(&input[i..i + n]);
                        self.held.push(0x1b);
                        self.state = State::Esc;
                        i += n + 1;
                    }
                    None => {
                        out.extend_from_slice(&input[i..]);
                        i = input.len();
                    }
                },
                State::Esc => {
                    let b = input[i];
                    i += 1;
                    if b == b'_' {
                        self.held.push(b);
                        self.state = State::Apc;
                    } else {
                        self.release(&mut out);
                        self.reprocess(b, &mut out);
                    }
                }
                State::Apc => {
                    let b = input[i];
                    i += 1;
                    if b == b'G' {
                        self.held.push(b);
                        self.state = State::Header;
                    } else {
                        self.release(&mut out);
                        self.reprocess(b, &mut out);
                    }
                }
                State::Header => {
                    let b = input[i];
                    i += 1;
                    match b {
                        b';' => {
                            if needs_rewrite(&self.held[3..]) {
                                self.held.push(b);
                                self.payload_at = self.held.len();
                                self.state = State::Capture;
                            } else {
                                self.held.push(b);
                                self.release(&mut out);
                                self.state = State::Pass;
                            }
                        }
                        0x1b | 0x07 | 0x18 | 0x1a => {
                            // No payload: nothing to read.
                            self.release(&mut out);
                            self.end_apc(b, &mut out);
                        }
                        _ => {
                            self.held.push(b);
                            if self.held.len() > MAX_HEADER {
                                self.release(&mut out);
                                self.state = State::Pass;
                            }
                        }
                    }
                }
                State::Pass => match apc_end(&input[i..]) {
                    Some(n) => {
                        out.extend_from_slice(&input[i..i + n]);
                        let b = input[i + n];
                        i += n + 1;
                        self.end_apc(b, &mut out);
                    }
                    None => {
                        out.extend_from_slice(&input[i..]);
                        i = input.len();
                    }
                },
                State::Capture => {
                    let b = input[i];
                    i += 1;
                    match b {
                        0x1b | 0x07 | 0x18 | 0x1a => {
                            match rewritten(&self.held[3..self.payload_at - 1], &self.held[self.payload_at..]) {
                                Some(seq) => out.extend_from_slice(&seq),
                                None => out.extend_from_slice(&self.held),
                            }
                            self.held.clear();
                            self.end_apc(b, &mut out);
                        }
                        _ => {
                            self.held.push(b);
                            if self.held.len() - self.payload_at > MAX_NAME {
                                self.release(&mut out);
                                self.state = State::Pass;
                            }
                        }
                    }
                }
            }
        }
        Cow::Owned(out)
    }

    fn release(&mut self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.held);
        self.held.clear();
    }

    /// A byte that ended `ESC` or `ESC _` without starting a kitty command.
    fn reprocess(&mut self, b: u8, out: &mut Vec<u8>) {
        if b == 0x1b {
            self.held.push(b);
            self.state = State::Esc;
        } else {
            out.push(b);
            self.state = State::Ground;
        }
    }

    /// The byte that ended an APC: ESC may start the ST (or anything else).
    fn end_apc(&mut self, b: u8, out: &mut Vec<u8>) {
        if b == 0x1b {
            self.held.push(b);
            self.state = State::Esc;
        } else {
            out.push(b);
            self.state = State::Ground;
        }
    }
}

/// Where an APC's payload ends: ESC (the ST, or an escape that cuts it
/// short), BEL, CAN or SUB, as in rio-vt's parser.
fn apc_end(bytes: &[u8]) -> Option<usize> {
    let a = memchr::memchr3(0x1b, 0x07, 0x18, bytes);
    let b = memchr::memchr(0x1a, a.map_or(bytes, |a| &bytes[..a]));
    b.or(a)
}

fn keys(header: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    header.split(|&b| b == b',').filter_map(|kv| match kv {
        [k, b'=', v @ ..] => Some((*k, v)),
        _ => None,
    })
}

fn needs_rewrite(header: &[u8]) -> bool {
    keys(header).any(|(k, v)| k == b't' && matches!(v, b"f" | b"t" | b"s"))
}

/// The command with its data inline (`ESC _ G keys ; base64`, without the
/// terminator, which follows in the stream), or None when the data can't
/// be read.
fn rewritten(header: &[u8], payload: &[u8]) -> Option<Vec<u8>> {
    let mut medium = b'd';
    let (mut size, mut offset) = (0u64, 0u64);
    for (k, v) in keys(header) {
        let num = || std::str::from_utf8(v).ok().and_then(|s| s.parse::<u64>().ok());
        match k {
            b't' => medium = *v.first()?,
            b'S' => size = num()?,
            b'O' => offset = num()?,
            _ => {}
        }
    }
    let name = rio_vt::simd_base64::decode(payload)?;
    let name = std::str::from_utf8(&name).ok()?;
    let data = match medium {
        b'f' => read_file(Path::new(name), offset, size, false)?,
        b't' => read_file(Path::new(name), offset, size, true)?,
        b's' => read_shm(name, offset, size)?,
        _ => return None,
    };
    // As it is (compressed only if it was): compressing raw frames cost
    // the daemon 4x the CPU of everything else per frame, and a client that
    // falls behind gets compressed snapshots instead (docs/stage0-findings.md).
    let mut out = String::with_capacity(header.len() + data.len() / 3 * 4 + 16);
    out.push_str("\x1b_G");
    let mut first = true;
    for kv in header.split(|&b| b == b',') {
        if matches!(kv.first(), Some(b't' | b'S' | b'O' | b'm')) && kv.get(1) == Some(&b'=') {
            continue;
        }
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str(std::str::from_utf8(kv).ok()?);
    }
    out.push_str(if first { "t=d" } else { ",t=d" });
    out.push(';');
    base64_into(&data, &mut out);
    Some(out.into_bytes())
}

/// What kitty won't read: devices and kernel interfaces.
fn sensitive(path: &Path) -> bool {
    path.starts_with("/proc") || path.starts_with("/sys") || (path.starts_with("/dev") && !path.starts_with("/dev/shm"))
}

fn in_temp_dir(path: &Path) -> bool {
    let Some(dir) = path.parent() else { return false };
    let mut temps = vec![std::env::temp_dir(), "/tmp".into(), "/dev/shm".into()];
    temps.extend(temps.clone().into_iter().filter_map(|d| d.canonicalize().ok()));
    temps.iter().any(|t| dir == t)
}

fn read_file(path: &Path, offset: u64, size: u64, temporary: bool) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    if !path.is_absolute() {
        return None;
    }
    let real = path.canonicalize().ok()?;
    if sensitive(path) || sensitive(&real) {
        return None;
    }
    if temporary && (!real.to_string_lossy().contains("tty-graphics-protocol") || !in_temp_dir(&real)) {
        return None;
    }
    let mut file = std::fs::File::open(&real).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = if size > 0 { size } else { meta.len().saturating_sub(offset) };
    if len > MAX_DATA || offset.checked_add(len)? > meta.len() {
        return None;
    }
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut data = Vec::with_capacity(len as usize);
    file.take(len).read_to_end(&mut data).ok()?;
    if temporary {
        let _ = std::fs::remove_file(&real);
    }
    Some(data)
}

#[cfg(unix)]
fn read_shm(name: &str, offset: u64, size: u64) -> Option<Vec<u8>> {
    // A POSIX shared memory name: one leading slash, no other.
    if name.is_empty() || name.len() > 255 || name[1..].contains('/') || name.contains('\0') {
        return None;
    }
    let c_name = std::ffi::CString::new(name).ok()?;
    // SAFETY: `c_name` is a valid NUL-terminated string.
    let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return None;
    }
    let data = map_and_copy(fd, offset, size);
    // SAFETY: `fd` is the descriptor shm_open returned, closed once here.
    unsafe { libc::close(fd) };
    // The protocol: the terminal removes the shared memory once read.
    // SAFETY: as above.
    unsafe { libc::shm_unlink(c_name.as_ptr()) };
    data
}

#[cfg(unix)]
fn map_and_copy(fd: libc::c_int, offset: u64, size: u64) -> Option<Vec<u8>> {
    // SAFETY: an all-zero `stat` is a valid value for fstat to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open and `st` is writable.
    if unsafe { libc::fstat(fd, &mut st) } != 0 {
        return None;
    }
    let total = u64::try_from(st.st_size).ok()?;
    let len = if size > 0 { size } else { total.saturating_sub(offset) };
    if len == 0 || len > MAX_DATA || offset.checked_add(len)? > total {
        return None;
    }
    let map_len = usize::try_from(offset + len).ok()?;
    // SAFETY: mapping `map_len` bytes of an open descriptor read-only; the
    // result is checked against MAP_FAILED before use.
    let ptr = unsafe { libc::mmap(std::ptr::null_mut(), map_len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
    if ptr == libc::MAP_FAILED {
        return None;
    }
    // SAFETY: the mapping is `map_len` bytes long and readable, and
    // `offset + len == map_len`.
    let data = unsafe { std::slice::from_raw_parts((ptr as *const u8).add(offset as usize), len as usize) }.to_vec();
    // SAFETY: unmapping exactly the mapping made above.
    unsafe { libc::munmap(ptr, map_len) };
    Some(data)
}

/// Shared memory on Windows is a named file mapping; not rewritten (the
/// command passes through and the terminal reads it itself).
#[cfg(not(unix))]
fn read_shm(_name: &str, _offset: u64, _size: u64) -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(r: &mut MediaRewriter, chunks: &[&[u8]]) -> Vec<u8> {
        chunks.iter().flat_map(|c| r.rewrite(c).into_owned()).collect()
    }

    fn b64(s: &[u8]) -> String {
        let mut out = String::new();
        base64_into(s, &mut out);
        out
    }

    #[test]
    fn plain_output_and_direct_transmissions_pass_unchanged() {
        let mut r = MediaRewriter::new();
        let text = b"hello \x1b[31mred\x1b[0m \x1b]0;title\x07";
        assert!(matches!(r.rewrite(text), Cow::Borrowed(_)));
        let direct = b"\x1b_Ga=T,f=32,s=1,v=1;AAAAAA==\x1b\\after";
        assert_eq!(all(&mut r, &[direct]), direct);
        // Split anywhere, the bytes come out the same.
        for cut in 0..direct.len() {
            let mut r = MediaRewriter::new();
            assert_eq!(all(&mut r, &[&direct[..cut], &direct[cut..]]), direct, "cut at {cut}");
        }
    }

    #[test]
    fn a_long_direct_payload_streams_without_buffering() {
        let mut r = MediaRewriter::new();
        let head = b"\x1b_Ga=T,f=32,s=100,v=100,m=1;";
        assert_eq!(&*r.rewrite(head), head);
        let payload = vec![b'A'; 64 * 1024];
        assert!(matches!(r.rewrite(&payload), Cow::Borrowed(_)), "payload chunks are passed through as they are");
        assert_eq!(&*r.rewrite(b"\x1b\\x"), b"\x1b\\x");
    }

    #[test]
    fn a_file_becomes_a_direct_transmission() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.rgba");
        std::fs::write(&path, [1u8, 2, 3, 4]).unwrap();
        let seq = format!("\x1b_Ga=T,t=f,f=32,s=1,v=1,i=5;{}\x1b\\", b64(path.to_str().unwrap().as_bytes()));
        let mut r = MediaRewriter::new();
        let out = String::from_utf8(all(&mut r, &[seq.as_bytes()])).unwrap();
        assert_eq!(out, format!("\x1b_Ga=T,f=32,s=1,v=1,i=5,t=d;{}\x1b\\", b64(&[1, 2, 3, 4])));
        assert!(out.ends_with("\x1b\\"));
        assert!(path.exists(), "a plain file stays");

        // And the terminal shows it.
        let size = crate::TermSize { cols: 10, rows: 4, cell_width: 8, cell_height: 16 };
        let (t, _ch) = crate::Terminal::new(size, true, std::sync::Arc::new(crate::Palette::default()));
        t.feed(out.as_bytes());
        let mut frame = crate::Frame::default();
        t.snapshot_into(&mut frame);
        assert_eq!(frame.images.len(), 1);
        assert_eq!(frame.images[0].image.rgba, [1, 2, 3, 4]);
    }

    #[test]
    fn a_temporary_file_is_read_once_and_removed() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("goatty-test-tty-graphics-protocol-{}.png", std::process::id()));
        std::fs::write(&path, b"\x89PNG not really").unwrap();
        let seq = format!("\x1b_Ga=t,t=t,f=100,i=9;{}\x1b\\", b64(path.to_str().unwrap().as_bytes()));
        let mut r = MediaRewriter::new();
        let out = String::from_utf8(all(&mut r, &[seq.as_bytes()])).unwrap();
        assert!(out.starts_with("\x1b_Ga=t,f=100,i=9,t=d;"), "PNG stays as it is: {out}");
        assert!(!path.exists(), "the temporary file is gone");
    }

    #[test]
    fn what_kitty_would_not_read_passes_through() {
        let mut r = MediaRewriter::new();
        for name in ["/proc/self/environ", "relative/path", "/nonexistent/file"] {
            let seq = format!("\x1b_Ga=T,t=f;{}\x1b\\", b64(name.as_bytes()));
            assert_eq!(all(&mut r, &[seq.as_bytes()]), seq.as_bytes(), "{name}");
        }
        // A file not named like a temporary one isn't deleted.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keep.png");
        std::fs::write(&path, b"x").unwrap();
        let seq = format!("\x1b_Ga=T,t=t;{}\x1b\\", b64(path.to_str().unwrap().as_bytes()));
        assert_eq!(all(&mut r, &[seq.as_bytes()]), seq.as_bytes());
        assert!(path.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn shared_memory_is_read_and_unlinked() {
        let name = format!("/goatty-test-{}", std::process::id());
        let c = std::ffi::CString::new(name.clone()).unwrap();
        // SAFETY: test setup with a valid name; the fd is closed below.
        unsafe {
            let fd = libc::shm_open(c.as_ptr(), libc::O_CREAT | libc::O_RDWR, 0o600);
            assert!(fd >= 0);
            assert_eq!(libc::write(fd, [9u8, 8, 7].as_ptr().cast(), 3), 3);
            libc::close(fd);
        }
        let seq = format!("\x1b_Ga=q,i=3,t=s,f=24,s=1,v=1,S=3;{}\x1b\\", b64(name.as_bytes()));
        let mut r = MediaRewriter::new();
        let out = String::from_utf8(all(&mut r, &[&seq.as_bytes()[..10], &seq.as_bytes()[10..]])).unwrap();
        assert_eq!(out, format!("\x1b_Ga=q,i=3,f=24,s=1,v=1,t=d;{}\x1b\\", b64(&[9, 8, 7])));
        // SAFETY: probing for the name; a successful open is closed.
        let gone = unsafe {
            let fd = libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0);
            if fd >= 0 {
                libc::close(fd);
                libc::shm_unlink(c.as_ptr());
            }
            fd < 0
        };
        assert!(gone, "the shared memory was unlinked");
    }
}

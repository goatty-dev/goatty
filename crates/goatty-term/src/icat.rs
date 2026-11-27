//! `goatty icat` / `goattyd icat` (and `icat` in goatty's shell): show
//! images in the terminal with the kitty graphics protocol, without kitty's
//! tools installed.
//!
//! Decodes PNG, JPEG, GIF (the first frame), WebP, BMP, TIFF, ICO and PNM,
//! scales an image down to fit the terminal (never up), and sends it inline
//! (`t=d`), zlib-compressed, in 4 KiB chunks as the protocol asks. Before
//! the first image it asks the terminal whether it shows kitty graphics,
//! unless it is goatty.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use image::{DynamicImage, ImageDecoder, ImageReader, RgbaImage};

/// What to show and how big.
#[derive(Debug, Default, Clone)]
pub struct Options {
    /// Image files; `-` reads one from standard input.
    pub files: Vec<PathBuf>,
    /// At most this many columns wide (default: the terminal's width).
    pub width: Option<u16>,
    /// At most this many rows high (default: the terminal's height, less
    /// one row so the prompt stays in view).
    pub height: Option<u16>,
    /// Send the images without asking the terminal first.
    pub force: bool,
}

/// The terminal's size in cells and the size of a cell in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub cols: u16,
    pub rows: u16,
    pub cell_width: u16,
    pub cell_height: u16,
}

impl Geometry {
    /// Standard output's terminal; a typical cell size where it reports
    /// none (or isn't a terminal).
    fn of_stdout() -> Self {
        let fallback = crate::TermSize { cols: 80, rows: 24, cell_width: 0, cell_height: 0 };
        #[cfg(unix)]
        let size = {
            use std::os::fd::AsFd;
            crate::pty::window_size(std::io::stdout().as_fd()).unwrap_or(fallback)
        };
        #[cfg(not(unix))]
        let size = fallback;
        let (cell_width, cell_height) =
            if size.cell_width > 0 && size.cell_height > 0 { (size.cell_width, size.cell_height) } else { (10, 20) };
        Self { cols: size.cols, rows: size.rows, cell_width, cell_height }
    }

    /// The largest image, in pixels, that fits `width` x `height` cells.
    pub fn fit(&self, width: Option<u16>, height: Option<u16>) -> (u32, u32) {
        let cols = width.unwrap_or(self.cols).max(1) as u32;
        let rows = height.unwrap_or(self.rows.saturating_sub(1)).max(1) as u32;
        (cols * self.cell_width as u32, rows * self.cell_height as u32)
    }
}

/// Show the images. Returns how many couldn't be shown (each is reported on
/// standard error).
pub fn run(opts: &Options) -> Result<usize> {
    anyhow::ensure!(!opts.files.is_empty(), "name an image file (or - for standard input)");
    if !opts.force && !supported()? {
        anyhow::bail!(
            "this terminal doesn't show images (kitty graphics protocol): use goatty, kitty, WezTerm or Ghostty, or pass --force to send them anyway"
        );
    }
    let geometry = Geometry::of_stdout();
    let max = geometry.fit(opts.width, opts.height);
    let mut out = std::io::stdout().lock();
    let mut failed = 0;
    for file in &opts.files {
        let shown = load(file).map(|img| fit(img, max)).and_then(|img| {
            out.write_all(&encode(&img, image_id()))?;
            out.write_all(b"\n")?;
            out.flush()?;
            Ok(())
        });
        if let Err(e) = shown {
            eprintln!("icat: {}: {e:#}", file.display());
            failed += 1;
        }
    }
    Ok(failed)
}

fn load(file: &Path) -> Result<RgbaImage> {
    let reader = if file == Path::new("-") {
        let mut data = Vec::new();
        std::io::stdin().read_to_end(&mut data).context("reading standard input")?;
        ImageReader::new(std::io::Cursor::new(data)).with_guessed_format()?.into_decoder().map(decode)
    } else {
        ImageReader::open(file)?.with_guessed_format()?.into_decoder().map(decode)
    };
    Ok(reader.context("not an image goatty can read")??.into_rgba8())
}

/// Decode, turned the way the camera says (EXIF orientation).
fn decode(mut decoder: impl ImageDecoder) -> Result<DynamicImage> {
    let orientation = decoder.orientation()?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

/// Scaled down to fit `max` (width, height) in pixels, keeping its shape.
pub fn fit(img: RgbaImage, (max_w, max_h): (u32, u32)) -> RgbaImage {
    let (w, h) = img.dimensions();
    if w <= max_w && h <= max_h {
        return img;
    }
    let scale = (max_w as f64 / w as f64).min(max_h as f64 / h as f64);
    let (nw, nh) = (((w as f64 * scale).round() as u32).max(1), ((h as f64 * scale).round() as u32).max(1));
    image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle)
}

/// A fresh image id: random, so two runs don't replace each other's image.
fn image_id() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    // Ids above 2^31 look like a terminal's own to some; 0 means "none".
    (h.finish() as u32 & 0x7fff_ffff).max(1)
}

/// The kitty graphics commands that transmit and show `img` at the cursor
/// (the cursor moves past it), in chunks of at most 4096 base64 bytes.
pub fn encode(img: &RgbaImage, id: u32) -> Vec<u8> {
    use base64::Engine as _;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    let _ = z.write_all(img.as_raw());
    let data = base64::engine::general_purpose::STANDARD.encode(z.finish().unwrap_or_default());
    let mut out = Vec::with_capacity(data.len() + data.len() / 4096 * 16 + 64);
    let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        if i == 0 {
            let (w, h) = img.dimensions();
            let _ = write!(out, "\x1b_Ga=T,q=2,f=32,o=z,i={id},s={w},v={h},m={more};");
        } else {
            let _ = write!(out, "\x1b_Gm={more};");
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

/// Whether the terminal shows kitty graphics. goatty does; others are asked
/// (a query, then a device attributes request every terminal answers).
fn supported() -> Result<bool> {
    // Set by goatty for its shells (and kept by `ssh`-less nesting only).
    if std::env::var_os("TERM_PROGRAM").is_some_and(|p| p == "goatty") {
        return Ok(true);
    }
    #[cfg(unix)]
    {
        query::ask()
    }
    #[cfg(not(unix))]
    Ok(true)
}

#[cfg(unix)]
mod query {
    use std::io::Write;
    use std::time::{Duration, Instant};

    use anyhow::Result;

    /// Ask the terminal on standard input/output. Not a terminal (output
    /// into a file or a pipe): nothing to ask, send the images.
    pub(super) fn ask() -> Result<bool> {
        // SAFETY: isatty only inspects the descriptors.
        if unsafe { libc::isatty(0) == 0 || libc::isatty(1) == 0 } {
            return Ok(true);
        }
        let raw = Raw::enter()?;
        let mut out = std::io::stdout().lock();
        out.write_all(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c")?;
        out.flush()?;
        let answer = read_until_da(Duration::from_secs(2));
        drop(raw);
        Ok(match answer {
            Some(reply) => graphics_ok(&reply),
            // No answer at all (a slow link): try anyway.
            None => true,
        })
    }

    /// What arrived up to the device attributes reply (`ESC [ ? … c`), or
    /// None when it didn't come in time.
    fn read_until_da(timeout: Duration) -> Option<Vec<u8>> {
        let end = Instant::now() + timeout;
        let mut got = Vec::new();
        let mut buf = [0u8; 256];
        while Instant::now() < end {
            // SAFETY: reading into a buffer of its stated length.
            let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
            if n > 0 {
                got.extend_from_slice(&buf[..n as usize]);
                if has_da(&got) {
                    return Some(got);
                }
            }
        }
        None
    }

    fn has_da(b: &[u8]) -> bool {
        b.windows(3).enumerate().any(|(i, w)| w == b"\x1b[?" && b[i + 3..].contains(&b'c'))
    }

    pub(super) fn graphics_ok(reply: &[u8]) -> bool {
        reply.windows(9).any(|w| w == b"_Gi=31;OK")
    }

    /// Standard input without echo or line editing, reads returning after
    /// 0.1 s; restored when dropped.
    struct Raw(libc::termios);

    impl Raw {
        fn enter() -> Result<Self> {
            // SAFETY: an all-zero termios is a valid value for tcgetattr to fill.
            let mut t: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: fd 0 is a terminal (checked) and `t` is writable.
            if unsafe { libc::tcgetattr(0, &mut t) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let saved = t;
            t.c_lflag &= !(libc::ICANON | libc::ECHO);
            t.c_cc[libc::VMIN] = 0;
            t.c_cc[libc::VTIME] = 1;
            // SAFETY: as above; `t` is a valid termios.
            if unsafe { libc::tcsetattr(0, libc::TCSANOW, &t) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(Self(saved))
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            // SAFETY: restoring the settings read in `enter`.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.0) };
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn replies_are_read_right() {
            assert!(graphics_ok(b"\x1b_Gi=31;OK\x1b\\\x1b[?62;22c"));
            assert!(!graphics_ok(b"\x1b_Gi=31;ENOTSUPPORTED:\x1b\\\x1b[?62;22c"));
            assert!(!graphics_ok(b"\x1b[?62;22c"));
            assert!(has_da(b"\x1b[?62;22c"));
            assert!(!has_da(b"\x1b_Gi=31;OK\x1b\\\x1b[?62"));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{Frame, Palette, TermSize, Terminal};

    fn checker(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| if (x + y) % 2 == 0 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) })
    }

    #[test]
    fn what_it_sends_shows_the_image_and_moves_below_it() {
        // Noise doesn't compress: several chunks.
        let mut seed = 1u32;
        let mut img = RgbaImage::from_fn(300, 200, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            image::Rgba((seed >> 8).to_le_bytes())
        });
        img.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        img.put_pixel(1, 0, image::Rgba([0, 0, 255, 255]));
        let bytes = encode(&img, 77);
        assert!(bytes.windows(4).filter(|w| w == b"\x1b_Gm").count() > 10, "chunked");
        assert!(bytes.split(|&b| b == 0x1b).all(|seq| seq.len() <= 4096 + 64), "chunks of at most 4096 bytes");
        let (t, _ch) = Terminal::new(TermSize { cols: 80, rows: 24, cell_width: 10, cell_height: 20 }, true, Arc::new(Palette::default()));
        t.feed(&bytes);
        t.feed(b"\r\nnext");
        let mut f = Frame::default();
        t.snapshot_into(&mut f);
        assert_eq!(f.images.len(), 1);
        let i = &f.images[0];
        assert_eq!((i.image.width, i.image.height), (300, 200));
        assert_eq!((i.x, i.y, i.width, i.height), (0.0, 0.0, 30.0, 10.0));
        assert_eq!(&i.image.rgba[..8], &[255, 0, 0, 255, 0, 0, 255, 255]);
        let row = |r: usize| f.row(r).iter().map(|c| c.ch).collect::<String>().trim_end().to_string();
        assert_eq!(row(10), "next", "the text after it starts below the image");
    }

    #[test]
    fn images_shrink_to_fit_and_never_grow() {
        let g = Geometry { cols: 80, rows: 25, cell_width: 10, cell_height: 20 };
        assert_eq!(g.fit(None, None), (800, 480));
        assert_eq!(g.fit(Some(20), Some(5)), (200, 100));
        assert_eq!(fit(checker(1600, 400), g.fit(None, None)).dimensions(), (800, 200));
        assert_eq!(fit(checker(100, 1000), g.fit(None, None)).dimensions(), (48, 480));
        assert_eq!(fit(checker(30, 20), g.fit(None, None)).dimensions(), (30, 20));
    }

    #[test]
    fn files_in_any_common_format_load() {
        let dir = tempfile::tempdir().unwrap();
        for ext in ["png", "jpg", "gif", "bmp", "webp"] {
            let path = dir.path().join(format!("a.{ext}"));
            DynamicImage::ImageRgba8(checker(8, 6)).to_rgb8().save(&path).unwrap();
            assert_eq!(load(&path).unwrap().dimensions(), (8, 6), "{ext}");
        }
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "not an image").unwrap();
        assert!(load(&text).is_err());
    }
}

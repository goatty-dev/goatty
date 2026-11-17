//! Unpacking plugin package downloads ([`AssetFormat`]): `.tar.gz`,
//! `.tar.xz`, `.tar.zst`, `.tar` and `.zip`. A raw executable
//! ([`AssetFormat::Binary`]) is not an archive and is placed by the caller.
//!
//! Callers check the download's sha256 before anything here runs. Entries
//! never land outside the destination: `tar` and `zip` both refuse `..` and
//! absolute paths, and zip symlinks are only created when they stay inside.
//! Tar permissions are not kept; zip entries keep their Unix modes. Either
//! way the installer marks the package's executables itself.

use std::io::{Cursor, Read};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::manifest::AssetFormat;

/// Unpack `bytes` (in `format`) into `dest`, which must exist.
pub fn unpack(format: AssetFormat, bytes: &[u8], dest: &Path) -> Result<()> {
    match format {
        AssetFormat::Zip => {
            let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).context("read the .zip")?;
            zip.extract(dest).context("unpack the .zip")?;
        }
        AssetFormat::Binary => bail!("a \"binary\" download is not an archive"),
        tar_format => {
            let mut ar = tar::Archive::new(decoder(tar_format, bytes)?);
            ar.set_preserve_permissions(false);
            ar.unpack(dest).with_context(|| format!("unpack the .{}", tar_format.name()))?;
        }
    }
    Ok(())
}

/// The executables in an archive (files with an execute bit), at most
/// `limit`, for `goatty-plugin hash`. Empty for formats without modes.
pub fn executables(format: AssetFormat, bytes: &[u8], limit: usize) -> Result<Vec<String>> {
    let mut out = Vec::new();
    match format {
        AssetFormat::Binary => {}
        AssetFormat::Zip => {
            let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).context("read the .zip")?;
            for i in 0..zip.len() {
                let f = zip.by_index_raw(i)?;
                if out.len() < limit && f.is_file() && f.unix_mode().is_some_and(|m| m & 0o111 != 0) {
                    out.push(f.name()?.into_owned());
                }
            }
        }
        tar_format => {
            let mut ar = tar::Archive::new(decoder(tar_format, bytes)?);
            for entry in ar.entries()?.flatten() {
                let h = entry.header();
                if out.len() < limit && h.entry_type().is_file() && h.mode().is_ok_and(|m| m & 0o111 != 0) {
                    out.push(entry.path().map(|p| p.display().to_string()).unwrap_or_default());
                }
            }
        }
    }
    Ok(out)
}

/// The decompressed tar stream of a tar format.
fn decoder<'a>(format: AssetFormat, bytes: &'a [u8]) -> Result<Box<dyn Read + 'a>> {
    Ok(match format {
        AssetFormat::TarGz => Box::new(flate2::read::GzDecoder::new(bytes)),
        AssetFormat::TarXz => Box::new(lzma_rust2::XzReader::new(bytes, true)),
        AssetFormat::TarZst => Box::new(zstd::stream::read::Decoder::with_buffer(bytes).context("read the .tar.zst")?),
        AssetFormat::Tar => Box::new(bytes),
        AssetFormat::Zip | AssetFormat::Binary => bail!("{} is not a tar format", format.name()),
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;

    /// A tar of `(path, contents, mode)`.
    pub fn tar(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for (path, data, mode) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_cksum();
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap()
    }

    /// `files` packed in `format`.
    pub fn pack(format: AssetFormat, files: &[(&str, &[u8], u32)]) -> Vec<u8> {
        match format {
            AssetFormat::Tar => tar(files),
            AssetFormat::TarGz => {
                let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
                e.write_all(&tar(files)).unwrap();
                e.finish().unwrap()
            }
            AssetFormat::TarXz => {
                let mut w = lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::default()).unwrap();
                w.write_all(&tar(files)).unwrap();
                w.finish().unwrap()
            }
            AssetFormat::TarZst => zstd::stream::encode_all(&tar(files)[..], 1).unwrap(),
            AssetFormat::Zip => {
                let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
                for (path, data, mode) in files {
                    let opts = zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Deflated)
                        .unix_permissions(*mode);
                    z.start_file(*path, opts).unwrap();
                    z.write_all(data).unwrap();
                }
                z.finish().unwrap().into_inner()
            }
            AssetFormat::Binary => files[0].1.to_vec(),
        }
    }

    const ARCHIVES: [AssetFormat; 5] = [AssetFormat::TarGz, AssetFormat::TarXz, AssetFormat::TarZst, AssetFormat::Tar, AssetFormat::Zip];

    #[test]
    fn unpacks_every_archive_format() {
        for format in ARCHIVES {
            let bytes = pack(format, &[("t-1/bin/tool", b"#!/bin/sh\n", 0o755), ("t-1/README", b"hi", 0o644)]);
            let dir = tempfile::tempdir().unwrap();
            unpack(format, &bytes, dir.path()).unwrap_or_else(|e| panic!("{format:?}: {e:#}"));
            assert_eq!(std::fs::read(dir.path().join("t-1/bin/tool")).unwrap(), b"#!/bin/sh\n", "{format:?}");
            assert_eq!(std::fs::read(dir.path().join("t-1/README")).unwrap(), b"hi", "{format:?}");
            assert_eq!(executables(format, &bytes, 10).unwrap(), ["t-1/bin/tool"], "{format:?}");
        }
    }

    #[test]
    fn refuses_garbage_and_wrong_formats() {
        let zip = pack(AssetFormat::Zip, &[("a", b"a", 0o644)]);
        let dir = tempfile::tempdir().unwrap();
        for format in [AssetFormat::TarGz, AssetFormat::TarXz, AssetFormat::TarZst] {
            assert!(unpack(format, &zip, dir.path()).is_err(), "{format:?} of a zip");
        }
        assert!(unpack(AssetFormat::Zip, &pack(AssetFormat::TarGz, &[("a", b"a", 0o644)]), dir.path()).is_err());
        assert!(unpack(AssetFormat::Binary, b"x", dir.path()).is_err());
    }

    #[test]
    fn stays_inside_the_destination() {
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        z.start_file("../escape", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"x").unwrap();
        let bytes = z.finish().unwrap().into_inner();
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("dest");
        std::fs::create_dir(&dest).unwrap();
        let _ = unpack(AssetFormat::Zip, &bytes, &dest);
        assert!(!parent.path().join("escape").exists());
    }
}

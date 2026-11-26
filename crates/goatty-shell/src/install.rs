//! User-space installs of plugin packages (the built-in plugins' tools
//! and registry plugins' programs alike).
//!
//! An install checks the download against the manifest's sha256, unpacks
//! it (any `AssetFormat`: tar.gz/xz/zst, zip, or a raw executable) into `tools/<id>/<version>/` (staged, then renamed), and switches the
//! `bin/<exe>` and `plugins/<id>` symlinks atomically. No sudo, nothing
//! outside goatty's data directory.
//!
//! Downloads go through `curl` (or `wget`): the checksum, not the transport,
//! is what makes an archive trusted, so goattyd needs no TLS stack of its own.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use goatty_plugin::manifest::AssetFormat;

use crate::dirs::Dirs;

/// Download `url` with curl (or wget) through a temp file under `cache`.
pub fn download(url: &str, cache: &Path) -> Result<Vec<u8>> {
    goatty_plugin::fetch::download(url, cache)
}

/// Lowercase hex sha256.
pub fn sha256_hex(bytes: &[u8]) -> String {
    goatty_plugin::bundle::sha256_hex(bytes)
}

/// A plugin package to install (`[[package]]` with the asset for this host).
pub struct PackageInstall<'a> {
    pub id: &'a str,
    pub version: &'a str,
    pub sha256: &'a str,
    pub format: AssetFormat,
    pub root: &'a str,
    /// Paths of the executables relative to `root`; linked as `bin/<file name>`.
    pub bins: &'a [String],
    /// More names for the one executable (`[[package]] applets`), each
    /// linked to it as `bin/<name>` (`<name>.exe` on Windows).
    pub applets: &'a [String],
    /// A shell script relative to `root`: the package directory is linked as
    /// `plugins/<id>`.
    pub source: Option<&'a str>,
    /// The command that prints the tool's completions for this host's
    /// managed shell ([`Completions::for_shell`](goatty_plugin::manifest::Completions::for_shell));
    /// empty for none.
    pub completions: &'a [String],
}

/// Executable file name placed in `bin/` for a manifest `bin` entry.
/// On Windows, appends `.exe` when the name doesn't already have an extension.
pub fn bin_file_name(bin: &str) -> String {
    let name = goatty_plugin::manifest::bin_name(bin);
    #[cfg(windows)]
    {
        if Path::new(name).extension().is_none() {
            return format!("{name}.exe");
        }
    }
    name.to_owned()
}

/// Resolve an extracted executable inside `root` for manifest entry `b`.
/// On Windows, falls back to `{b}.exe` when `b` omits the extension.
fn locate_extracted_bin(root: &Path, b: &str) -> PathBuf {
    let direct = root.join(b);
    #[cfg(windows)]
    {
        if !direct.exists() && Path::new(b).extension().is_none() {
            let with_exe = root.join(format!("{b}.exe"));
            if with_exe.exists() {
                return with_exe;
            }
        }
    }
    direct
}

#[cfg(windows)]
fn owner_file(dirs: &Dirs, file_name: &str) -> PathBuf {
    dirs.bin_dir().join(format!(".{file_name}.goatty"))
}

#[cfg(windows)]
fn write_owner(dirs: &Dirs, file_name: &str, id: &str, version: &str) -> Result<()> {
    let path = owner_file(dirs, file_name);
    let tmp = dirs.bin_dir().join(format!(".{file_name}.goatty.tmp-{}", std::process::id()));
    std::fs::write(&tmp, format!("{id}\n{version}\n")).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

/// Read `(package_id, version)` from the Windows ownership sidecar for `bin/<file_name>`.
#[cfg(windows)]
pub(crate) fn read_bin_owner(dirs: &Dirs, file_name: &str) -> Option<(String, String)> {
    let text = std::fs::read_to_string(owner_file(dirs, file_name)).ok()?;
    let mut lines = text.lines();
    let id = lines.next()?.trim();
    let version = lines.next()?.trim();
    (!id.is_empty() && !version.is_empty()).then(|| (id.to_owned(), version.to_owned()))
}

/// Verify and unpack a plugin package into `tools/<id>/<version>/`, link
/// its executables into `bin/` and its directory as `plugins/<id>` when it
/// has a script to source. A `bin/` link that belongs to something else
/// (another package, a user's file) is never replaced. Older versions of
/// the package are removed (running processes keep their open files).
/// The tool's completion function is generated last; a tool that can't
/// print it is still installed (logged). Returns the activated links.
pub fn install_package(p: &PackageInstall<'_>, archive: &[u8], dirs: &Dirs) -> Result<Vec<PathBuf>> {
    let got = sha256_hex(archive);
    if !got.eq_ignore_ascii_case(p.sha256) {
        bail!("checksum mismatch for {} {}: expected {}, got {got}; not installing", p.id, p.version, p.sha256);
    }
    let names: Vec<String> = p.bins.iter().map(|b| bin_file_name(b)).collect();
    let applets: Vec<String> = p.applets.iter().map(|a| bin_file_name(a)).collect();
    if !applets.is_empty() && names.len() != 1 {
        bail!("{}: applets need exactly one program to link to", p.id);
    }
    let tool_dir = dirs.tools_dir().join(p.id);
    for name in names.iter().chain(&applets) {
        if let Some(owner) = link_owner(dirs, name)
            && owner != p.id
        {
            bail!("{name} is already provided by {owner}; not replacing it");
        }
    }

    std::fs::create_dir_all(&tool_dir).with_context(|| format!("create {}", tool_dir.display()))?;
    let staging = tool_dir.join(format!(".{}.partial-{}", p.version, std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)?;
    let mut extracted_rel: Vec<PathBuf> = Vec::with_capacity(p.bins.len());
    let result = (|| -> Result<()> {
        let base = staging.join(p.root);
        if p.format == AssetFormat::Binary {
            let dest = base.join(&names[0]);
            std::fs::create_dir_all(dest.parent().context("no parent")?)?;
            std::fs::write(&dest, archive)?;
        } else {
            goatty_plugin::archive::unpack(p.format, archive, &staging).context("unpack the download")?;
        }
        for b in p.bins {
            let exe = locate_extracted_bin(&base, b);
            let meta = std::fs::symlink_metadata(&exe).with_context(|| format!("the download has no {}/{b}", p.root))?;
            if !meta.is_file() {
                bail!("{}/{b} in the download is not a regular file", p.root);
            }
            #[cfg(unix)]
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
            extracted_rel.push(exe.strip_prefix(&staging).unwrap_or(&exe).to_path_buf());
        }
        if let Some(src) = p.source {
            let meta = std::fs::symlink_metadata(base.join(src)).with_context(|| format!("the download has no {}/{src}", p.root))?;
            if !meta.is_file() {
                bail!("{}/{src} in the download is not a regular file", p.root);
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let version_dir = tool_dir.join(p.version);
    if version_dir.exists() {
        std::fs::remove_dir_all(&version_dir).with_context(|| format!("remove old {}", version_dir.display()))?;
    }
    std::fs::rename(&staging, &version_dir)?;
    let mut links = Vec::new();
    // Each applet is another name for the one program.
    let applet_links = extracted_rel.first().into_iter().flat_map(|rel| applets.iter().map(move |a| (rel, a)));
    for (rel, name) in extracted_rel.iter().zip(&names).chain(applet_links) {
        let link = dirs.bin_dir().join(name);
        let target = version_dir.join(rel);
        #[cfg(unix)]
        replace_symlink(&target, &link)?;
        #[cfg(windows)]
        {
            crate::winlink::replace_bin(&target, &link)?;
            write_owner(dirs, name, p.id, p.version)?;
        }
        links.push(link);
    }
    if p.source.is_some() {
        let link = dirs.plugins_dir().join(p.id);
        replace_dir_link(&version_dir.join(p.root), &link)?;
        links.push(link);
    }
    for entry in std::fs::read_dir(&tool_dir)?.flatten() {
        if entry.file_name() != p.version {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    if let Some((name, args)) = p.completions.split_first() {
        let cmd_name = goatty_plugin::manifest::command_name(name);
        let exe = dirs.bin_dir().join(bin_file_name(name));
        if let Err(e) = crate::completions::for_package(dirs, cmd_name, &exe, args) {
            log::warn!("no completions for {}: {e:#}", p.id);
        }
    }
    for name in &names {
        invalidate_init_cache(dirs, goatty_plugin::manifest::command_name(name));
    }
    Ok(links)
}

/// Drop cached init scripts for executable `name` (if any), so a newly
/// installed, updated, or removed managed copy doesn't leave stale cache files.
fn invalidate_init_cache(dirs: &Dirs, name: &str) {
    let init_dir = dirs.init_cache_dir();
    let _ = std::fs::remove_file(init_dir.join(format!("{name}.zsh.zwc")));
    let _ = std::fs::remove_file(init_dir.join(format!("{name}.zsh")));
    let _ = std::fs::remove_file(init_dir.join(format!("{name}.ps1")));
    if let Ok(entries) = std::fs::read_dir(&init_dir) {
        let prefix = format!("{name}-");
        let mut variants: Vec<PathBuf> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let s = name.to_string_lossy();
                (s.starts_with(&prefix) && (s.ends_with(".zsh") || s.ends_with(".zsh.zwc") || s.ends_with(".ps1"))).then(|| e.path())
            })
            .collect();
        // Remove `.zsh.zwc` before `.zsh` so a concurrent reader never sees a stale `.zwc`.
        variants.sort_by_key(|p| !p.extension().is_some_and(|e| e == "zwc"));
        for path in variants {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Which managed tool or package `bin/<name>` belongs to: the `<id>` of
/// `tools/<id>/...` it points into, `"?"` for anything else that exists.
pub fn link_owner(dirs: &Dirs, name: &str) -> Option<String> {
    let file_name = bin_file_name(name);
    let link = dirs.bin_dir().join(&file_name);
    link.symlink_metadata().ok()?;
    #[cfg(unix)]
    {
        let target = std::fs::read_link(&link).ok();
        let owner = target
            .as_deref()
            .and_then(|t| t.strip_prefix(dirs.tools_dir()).ok())
            .and_then(|rel| rel.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        Some(owner.unwrap_or_else(|| "?".into()))
    }
    #[cfg(windows)]
    {
        Some(read_bin_owner(dirs, &file_name).map(|(id, _)| id).unwrap_or_else(|| "?".into()))
    }
}

/// Remove a plugin package: its `bin/` links and their completion functions,
/// its `plugins/<id>` link (only those pointing into its own directory) and
/// `tools/<id>/`. Returns
/// whether there was anything to remove.
pub fn uninstall_package(id: &str, bins: &[String], dirs: &Dirs) -> Result<bool> {
    let mut had = remove_links(id, bins, dirs)?;
    let tool_dir = dirs.tools_dir().join(id);
    let plugin_link = dirs.plugins_dir().join(id);
    #[cfg(unix)]
    let points_here = std::fs::read_link(&plugin_link).is_ok_and(|t| t.starts_with(&tool_dir));
    #[cfg(windows)]
    let points_here = crate::winlink::read_junction(&plugin_link).is_ok_and(|t| t.starts_with(crate::winlink::strip_verbatim(&tool_dir)));
    if points_here {
        #[cfg(unix)]
        std::fs::remove_file(&plugin_link).with_context(|| format!("remove {}", plugin_link.display()))?;
        #[cfg(windows)]
        crate::winlink::remove_junction(&plugin_link).with_context(|| format!("remove {}", plugin_link.display()))?;
        had = true;
    }
    if tool_dir.exists() {
        std::fs::remove_dir_all(&tool_dir).with_context(|| format!("remove {}", tool_dir.display()))?;
        had = true;
    }
    Ok(had)
}

/// Remove the `bin/` links (and completions) of package `id` among `bins`,
/// keeping the package itself. True when there was one.
pub fn remove_links(id: &str, bins: &[String], dirs: &Dirs) -> Result<bool> {
    let mut had = false;
    for b in bins {
        let file_name = bin_file_name(b);
        let cmd_name = goatty_plugin::manifest::command_name(b);
        if link_owner(dirs, &file_name).as_deref() == Some(id) {
            std::fs::remove_file(dirs.bin_dir().join(&file_name)).with_context(|| format!("remove bin/{file_name}"))?;
            #[cfg(windows)]
            let _ = std::fs::remove_file(owner_file(dirs, &file_name));
            crate::completions::remove(dirs, cmd_name)?;
            invalidate_init_cache(dirs, cmd_name);
            had = true;
        }
    }
    Ok(had)
}

/// Point directory `link` at `target`, atomically (symlink on Unix, NTFS junction on Windows).
fn replace_dir_link(target: &Path, link: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        replace_symlink(target, link)
    }
    #[cfg(windows)]
    {
        crate::winlink::replace_junction(target, link)
    }
}

/// Point `link` at `target`, atomically (symlink beside it, then rename).
#[cfg(unix)]
fn replace_symlink(target: &Path, link: &Path) -> Result<()> {
    let dir = link.parent().context("link has no parent")?;
    std::fs::create_dir_all(dir)?;
    let name = link.file_name().context("link has no name")?.to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).with_context(|| format!("symlink {}", tmp.display()))?;
    std::fs::rename(&tmp, link).with_context(|| format!("activate {}", link.display()))?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn targz(files: &[(&str, &[u8], u32)]) -> Vec<u8> {
        let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
        for (path, data, mode) in files {
            let mut h = tar::Header::new_gnu();
            h.set_size(data.len() as u64);
            h.set_mode(*mode);
            h.set_cksum();
            b.append_data(&mut h, path, *data).unwrap();
        }
        b.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn installs_zip_packages() {
        // How macOS builds usually ship (fzf's darwin release): a .zip whose
        // executable has no execute bit recorded.
        use std::io::Write;
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        z.start_file("fzf", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"#!/bin/sh\necho fzf\n").unwrap();
        let archive = z.finish().unwrap().into_inner();
        let sha = sha256_hex(&archive);
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let bins = vec!["fzf".to_string()];
        let p = PackageInstall {
            id: "fzf",
            version: "1",
            sha256: &sha,
            format: AssetFormat::Zip,
            root: "",
            bins: &bins,
            applets: &[],
            source: None,
            completions: &[],
        };
        assert_eq!(install_package(&p, &archive, &dirs).unwrap(), [dirs.bin_dir().join("fzf")]);
        // Another test thread forking while the binary was open for writing
        // holds that fd until its exec: retry ETXTBSY ("text file busy").
        let run = || std::process::Command::new(dirs.bin_dir().join("fzf")).output();
        let mut out = run();
        for _ in 0..100 {
            match &out {
                Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    out = run();
                }
                _ => break,
            }
        }
        let out = out.unwrap();
        assert_eq!(out.stdout, b"fzf\n");

        // The wrong format fails without touching the installed version.
        let wrong = PackageInstall { format: AssetFormat::TarXz, version: "2", ..p };
        assert!(install_package(&wrong, &archive, &dirs).is_err());
        assert!(dirs.tools_dir().join("fzf/1/fzf").is_file());
    }

    #[test]
    fn installs_zsh_plugin_packages() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let archive = targz(&[("p-1/demo.zsh", b"demo_loaded=1\n", 0o644), ("p-1/src/x.zsh", b"", 0o644)]);
        let sha = sha256_hex(&archive);
        let p = PackageInstall {
            id: "demo",
            version: "1",
            sha256: &sha,
            format: AssetFormat::TarGz,
            root: "p-1",
            bins: &[],
            applets: &[],
            source: Some("demo.zsh"),
            completions: &[],
        };
        let links = install_package(&p, &archive, &dirs).unwrap();
        assert_eq!(links, [dirs.plugins_dir().join("demo")]);
        assert!(dirs.plugins_dir().join("demo/demo.zsh").is_file(), "plugins/demo/demo.zsh resolves through the link");
        assert!(dirs.plugins_dir().join("demo/src/x.zsh").is_file(), "the whole package is there");

        let missing = PackageInstall { source: Some("nope.zsh"), version: "2", ..p };
        assert!(install_package(&missing, &archive, &dirs).unwrap_err().to_string().contains("nope.zsh"));
        assert!(dirs.plugins_dir().join("demo/demo.zsh").is_file(), "the failed install left version 1 alone");

        assert!(uninstall_package("demo", &[], &dirs).unwrap());
        assert!(dirs.plugins_dir().join("demo").symlink_metadata().is_err());
        assert!(!dirs.tools_dir().join("demo").exists());
        assert!(!uninstall_package("demo", &[], &dirs).unwrap());
    }

    #[test]
    fn installs_and_removes_completions() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let archive = targz(&[("t/tool", b"#!/bin/sh\n[ \"$1\" = zsh ] && printf '#compdef tool\\n'\n", 0o755)]);
        let sha = sha256_hex(&archive);
        let bins = vec!["tool".to_string()];
        let completions = vec!["tool".to_string(), "zsh".to_string()];
        let p = PackageInstall {
            id: "tool",
            version: "1",
            sha256: &sha,
            format: AssetFormat::TarGz,
            root: "t",
            bins: &bins,
            applets: &[],
            source: None,
            completions: &completions,
        };
        install_package(&p, &archive, &dirs).unwrap();
        let f = crate::completions::file(&dirs, "tool");
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "#compdef tool\n");

        // A tool that can't print them is still installed.
        let wrong = vec!["tool".to_string(), "bash".to_string()];
        install_package(&PackageInstall { version: "2", completions: &wrong, ..p }, &archive, &dirs).unwrap();
        assert!(dirs.bin_dir().join("tool").exists());

        uninstall_package("tool", &bins, &dirs).unwrap();
        assert!(!f.exists(), "removed with the package");
    }

    #[test]
    fn installs_plugin_packages_without_clobbering() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let archive = targz(&[("eza", b"#!/bin/sh\necho eza\n", 0o644)]);
        let sha = sha256_hex(&archive);
        let bins = vec!["eza".to_string()];
        let p = PackageInstall {
            id: "eza",
            version: "0.23.4",
            sha256: &sha,
            format: AssetFormat::TarGz,
            root: "",
            bins: &bins,
            applets: &[],
            source: None,
            completions: &[],
        };
        let links = install_package(&p, &archive, &dirs).unwrap();
        assert_eq!(links, [dirs.bin_dir().join("eza")]);
        assert_eq!(std::fs::metadata(&links[0]).unwrap().permissions().mode() & 0o777, 0o755);
        assert_eq!(link_owner(&dirs, "eza").as_deref(), Some("eza"));

        // Another package can't take over bin/eza; a wrong checksum installs nothing.
        let other = PackageInstall { id: "other", ..p };
        assert!(install_package(&other, &archive, &dirs).unwrap_err().to_string().contains("already provided by eza"));
        let bad = PackageInstall { sha256: "00", ..p };
        assert!(install_package(&bad, &archive, &dirs).is_err());

        // A raw binary download.
        let raw = b"\x7fELF...";
        let sha_raw = sha256_hex(raw);
        let bins_raw = vec!["tool".to_string()];
        let r = PackageInstall {
            id: "tool",
            version: "1",
            sha256: &sha_raw,
            format: AssetFormat::Binary,
            root: "",
            bins: &bins_raw,
            applets: &[],
            source: None,
            completions: &[],
        };
        install_package(&r, raw, &dirs).unwrap();
        assert_eq!(std::fs::read(dirs.bin_dir().join("tool")).unwrap(), raw);

        uninstall_package("eza", &bins, &dirs).unwrap();
        assert!(dirs.bin_dir().join("eza").symlink_metadata().is_err());
        assert!(!dirs.tools_dir().join("eza").exists());
        assert!(dirs.bin_dir().join("tool").exists(), "other packages untouched");
    }

    #[test]
    fn installs_and_uninstalls_invalidate_init_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let init_dir = dirs.init_cache_dir();
        std::fs::create_dir_all(&init_dir).unwrap();
        let seed = || {
            std::fs::write(init_dir.join("tool.zsh"), "# cached\n").unwrap();
            std::fs::write(init_dir.join("tool.zsh.zwc"), "zwc").unwrap();
            std::fs::write(init_dir.join("tool-dumb.zsh"), "# cached dumb\n").unwrap();
            std::fs::write(init_dir.join("tool-dumb.zsh.zwc"), "zwc").unwrap();
            std::fs::write(init_dir.join("other.zsh"), "# keep\n").unwrap();
        };

        let archive = targz(&[("t/tool", b"#!/bin/sh\necho v1\n", 0o755)]);
        let sha = sha256_hex(&archive);
        let bins = vec!["tool".to_string()];
        let p = PackageInstall {
            id: "tool",
            version: "1",
            sha256: &sha,
            format: AssetFormat::TarGz,
            root: "t",
            bins: &bins,
            applets: &[],
            source: None,
            completions: &[],
        };

        seed();
        install_package(&p, &archive, &dirs).unwrap();
        assert!(!init_dir.join("tool.zsh").exists());
        assert!(!init_dir.join("tool.zsh.zwc").exists());
        assert!(!init_dir.join("tool-dumb.zsh").exists());
        assert!(!init_dir.join("tool-dumb.zsh.zwc").exists());
        assert!(init_dir.join("other.zsh").exists());

        seed();
        assert!(uninstall_package("tool", &bins, &dirs).unwrap());
        assert!(!init_dir.join("tool.zsh").exists());
        assert!(!init_dir.join("tool.zsh.zwc").exists());
        assert!(!init_dir.join("tool-dumb.zsh").exists());
        assert!(!init_dir.join("tool-dumb.zsh.zwc").exists());
        assert!(init_dir.join("other.zsh").exists());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    #[test]
    fn installs_windows_zip_exe_with_owner_sidecar_and_invalidates_ps1_cache() {
        use std::io::Write;

        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        z.start_file("fzf.exe", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"MZfake-exe-payload").unwrap();
        let archive = z.finish().unwrap().into_inner();
        let sha = sha256_hex(&archive);

        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        let init_dir = dirs.init_cache_dir();
        std::fs::create_dir_all(&init_dir).unwrap();
        std::fs::write(init_dir.join("fzf-abc123.ps1"), "# cached\n").unwrap();
        std::fs::write(init_dir.join("other-abc123.ps1"), "# keep\n").unwrap();

        let bins = vec!["fzf".to_string()];
        let p = PackageInstall {
            id: "fzf",
            version: "0.74.5",
            sha256: &sha,
            format: AssetFormat::Zip,
            root: "",
            bins: &bins,
            applets: &[],
            source: None,
            completions: &[],
        };
        let installed = install_package(&p, &archive, &dirs).unwrap();
        assert_eq!(installed, [dirs.bin_dir().join("fzf.exe")]);
        assert_eq!(std::fs::read(dirs.bin_dir().join("fzf.exe")).unwrap(), b"MZfake-exe-payload");
        assert_eq!(link_owner(&dirs, "fzf").as_deref(), Some("fzf"));
        assert_eq!(read_bin_owner(&dirs, "fzf.exe"), Some(("fzf".into(), "0.74.5".into())));
        assert!(!init_dir.join("fzf-abc123.ps1").exists());
        assert!(init_dir.join("other-abc123.ps1").exists());

        // Another package cannot take over bin/fzf.exe.
        let other = PackageInstall { id: "other", ..p };
        assert!(install_package(&other, &archive, &dirs).unwrap_err().to_string().contains("already provided by fzf"));

        // Uninstall removes exe, sidecar, and tools dir.
        assert!(uninstall_package("fzf", &bins, &dirs).unwrap());
        assert!(!dirs.bin_dir().join("fzf.exe").exists());
        assert!(!owner_file(&dirs, "fzf.exe").exists());
        assert!(!dirs.tools_dir().join("fzf").exists());
    }
}

# Releasing

The version lives in one place: `version` under `[workspace.package]` in the root `Cargo.toml`. All crates inherit it. Binaries report it (`goatty --version`) and the remote install checks it. A release is that version tagged `v<version>`, for example `v0.2.0`. Versions follow semver. A version with a suffix such as `0.3.0-rc.1` is published as a GitHub pre-release.

## Cut a release

```sh
just bump 0.2.0        # = scripts/bump-version.sh 0.2.0
```

The script branches `release/v0.2.0` off `origin/main`, sets the version in `Cargo.toml` and `Cargo.lock`, pushes the branch and opens a pull request. When you merge the pull request, `.github/workflows/release.yml` sees the version change on `main` and does the rest:

1. **plan** reads the version and compares it with the previous `main` commit. If the version is unchanged, or its tag already exists, nothing else runs.
2. **remote-goattyd** builds static (musl) goattyds for x86_64 and aarch64 with cargo-zigbuild. goatty uploads these to remote hosts.
3. **linux** builds `goatty` and `goattyd` with the `dist` profile on ubuntu-22.04. The binaries then need glibc 2.35 or newer. The job runs `scripts/package.sh`, which makes the release archive and, with [nfpm](https://nfpm.goreleaser.com) (`packaging/nfpm.yaml`), the `.deb`, `.rpm` and Arch packages.
4. **test-packages** installs each package in a clean Ubuntu 22.04, Ubuntu 24.04, Debian 12, Fedora and Arch container and runs the binaries. A wrong dependency name or a missing library fails the release here.
   **test-aur** builds the AUR package (`packaging/aur/PKGBUILD`) with makepkg from this run's archive, then installs and runs it on a clean Arch.
5. **windows** builds `goatty.exe` and `goattyd.exe` with the `dist` profile on windows-latest, with the C runtime linked statically (a fresh Windows has no `vcruntime140.dll`; the job checks with `dumpbin`). `scripts/package-windows.ps1` makes the per-user MSI (WiX 5, `packaging/windows/goatty.wxs`), the portable `.zip` and `goattyd-x86_64-windows.exe` for remote Windows hosts. It also builds the same MSI as version 0.0.1 for the next step.
   **test-windows** runs `scripts/test-windows-install.ps1` on a clean runner: the `.zip` runs, the MSI upgrades the 0.0.1 install while its goattyd is running (no reboot, one installed product afterwards), puts goatty on `PATH` and in the Start Menu, and uninstalling removes all of it.
6. **publish** creates the tag on the merge commit and the GitHub release. The release notes start with install commands, followed by the pull requests merged since the previous release. The assets are the Linux and Windows packages, the goattyds for remote hosts and `SHA256SUMS`.
7. **aur** updates [goatty-bin](https://aur.archlinux.org/packages/goatty-bin) on the AUR for stable releases (not pre-releases). It needs the `AUR_SSH_PRIVATE_KEY` secret; without it, the job only prints a warning.

Pushing a `v*` tag by hand also publishes, but only if the tag matches the version in `Cargo.toml`. Tags created by the workflow's token don't start other workflows, so the merge never triggers a second run.

**Dry run:** go to Actions → Release → Run workflow and pick any branch. It builds and tests every package and uploads them as workflow artifacts, but publishes nothing. Do one after changing packaging, dependencies or the workflow itself.

## What gets installed

| | Packages (`/usr`) | Archive `install.sh` (`~/.local`) |
|---|---|---|
| `goatty`, `goattyd` | `/usr/bin` | `~/.local/bin` |
| static remote goattyds | `/usr/lib/goatty/remote` | `~/.local/lib/goatty/remote` |
| launcher, icon | `/usr/share/applications`, `/usr/share/icons/hicolor` | `$XDG_DATA_HOME` |
| bash/zsh/fish completions | yes | no (the managed zsh has them) |

On Windows, the MSI installs into `%LOCALAPPDATA%\Programs\goatty` without an Administrator prompt: `goatty.exe`, `goattyd.exe`, the static Linux goattyds in `remote\`, the licenses and the README. It adds that folder to the user's `PATH` and a Start Menu shortcut whose App User Model ID (`dev.goatty.goatty`) lets goatty show toast notifications. The `.zip` has the same files.

goatty finds the remote goattyds in `remote/` or `../lib/goatty/remote/` next to its binary (`goatty_client::remote::find_bundled`).

The `dist` profile is the release profile without debug info. The binary keeps its symbol names, so backtraces still name functions. On 2026-11-15 it shrank `goatty` from 327 MB to 66 MB (x86_64).

Package dependencies are listed by hand in `packaging/nfpm.yaml`. `ldd target/dist/goatty` shows the libraries the binary links directly. GPUI also loads libwayland-client, libwayland-egl and libvulkan at runtime. If one of these changes, update all three package formats and do a dry run.

## AUR: goatty-bin

goatty-bin installs the release archive into `/usr`. Its source of truth is `packaging/aur/PKGBUILD`. The aur job fills in `pkgver` and `sha256sums` (`scripts/aur-pkgbuild.sh`), writes `.SRCINFO`, and pushes both to `ssh://aur@aur.archlinux.org/goatty-bin.git`. Don't edit the AUR copy by hand: the next release overwrites it. To change dependencies or the install layout, change the file here and do a dry run, since test-aur builds it.

One-time setup, done by whoever maintains the package:

1. Create an account at https://aur.archlinux.org/register.
2. Make an SSH key just for publishing: `ssh-keygen -t ed25519 -N '' -C goatty-aur -f ~/.ssh/aur_goatty`.
3. Paste `~/.ssh/aur_goatty.pub` into *My Account → SSH Public Key* on the AUR.
4. Store the private key as a repository secret: `gh secret set AUR_SSH_PRIVATE_KEY -R goatty-dev/goatty < ~/.ssh/aur_goatty`.

The next stable release then creates goatty-bin, owned by that account. If a release has already happened without the secret, re-run its failed or skipped **aur** job from the Actions tab, or push the package by hand:

```sh
scripts/aur-pkgbuild.sh /tmp/goatty-bin goatty-<version>-x86_64-linux.tar.gz   # the release's archive
cd /tmp/goatty-bin && makepkg --printsrcinfo > .SRCINFO
git clone ssh://aur@aur.archlinux.org/goatty-bin.git aur && cp PKGBUILD .SRCINFO aur/
cd aur && git add PKGBUILD .SRCINFO && git commit -m "Update to <version>" && git push origin HEAD:master
```

## Windows installer

`packaging/windows/goatty.wxs` is a per-user MSI. Its `UpgradeCode` must never change: it is what makes a new MSI replace the installed version instead of installing a second copy. MSI versions are numeric, so a pre-release installs as its release's version (`0.3.0-rc.1` as `0.3.0`), and the same version may upgrade itself.

Windows can't replace a running `goattyd.exe`, and ConPTY sessions can't outlive their daemon. Before an upgrade or uninstall touches the files, the MSI runs the installed `goattyd.exe stop --force`, which ends its sessions. Uninstalling also runs `goattyd.exe service uninstall` to remove the Scheduled Task. goatty's settings and logs stay.

The icon, `packaging/windows/goatty.ico`, is made from `crates/goatty-app/assets/brand/goatty.svg` (16 to 48 px as bitmaps, 64 and 256 px as PNG). The `build.rs` of goatty-app and goatty-daemon embed it with the version info (`winresource`). GPUI uses that icon (resource ID 1) for the window. Regenerate the `.ico` when the logo changes.

The Windows builds aren't code-signed yet, so SmartScreen warns about them, and they aren't in winget or Scoop yet.

To build the installer on a Windows machine: install the .NET SDK, then WiX and its extensions at the version `release.yml` uses (`dotnet tool install --global wix --version 5.0.2`, `wix extension add --global WixToolset.Util.wixext/5.0.2` and `WixToolset.UI.wixext/5.0.2`), put the static goattyds from a release into `remote\`, then:

```powershell
cargo build --profile dist --locked -p goatty-app -p goatty-daemon
pwsh scripts/package-windows.ps1
```

Don't run `scripts/test-windows-install.ps1` on your own computer: it installs, upgrades and uninstalls goatty for your user.


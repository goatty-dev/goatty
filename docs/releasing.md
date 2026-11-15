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
5. **publish** creates the tag on the merge commit and the GitHub release. The release notes start with install commands, followed by the pull requests merged since the previous release. The assets are the packages, the static goattyds and `SHA256SUMS`.
6. **aur** updates [goatty-bin](https://aur.archlinux.org/packages/goatty-bin) on the AUR for stable releases (not pre-releases). It needs the `AUR_SSH_PRIVATE_KEY` secret; without it, the job only prints a warning.

Pushing a `v*` tag by hand also publishes, but only if the tag matches the version in `Cargo.toml`. Tags created by the workflow's token don't start other workflows, so the merge never triggers a second run.

**Dry run:** go to Actions → Release → Run workflow and pick any branch. It builds and tests every package and uploads them as workflow artifacts, but publishes nothing. Do one after changing packaging, dependencies or the workflow itself.

## What gets installed

| | Packages (`/usr`) | Archive `install.sh` (`~/.local`) |
|---|---|---|
| `goatty`, `goattyd` | `/usr/bin` | `~/.local/bin` |
| static remote goattyds | `/usr/lib/goatty/remote` | `~/.local/lib/goatty/remote` |
| launcher, icon | `/usr/share/applications`, `/usr/share/icons/hicolor` | `$XDG_DATA_HOME` |
| bash/zsh/fish completions | yes | no (the managed zsh has them) |

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

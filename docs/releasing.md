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
5. **publish** creates the tag on the merge commit and the GitHub release. The release notes start with install commands, followed by the pull requests merged since the previous release. The assets are the packages, the static goattyds and `SHA256SUMS`.

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

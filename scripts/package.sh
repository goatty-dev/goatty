#!/usr/bin/env bash
# Package a dist build into OUT_DIR (default dist/): the Linux release archive
# (install.sh into ~/.local) and, with nfpm on PATH, .deb, .rpm and Arch
# packages (packaging/nfpm.yaml). release.yml runs this; locally:
#   cargo build --profile dist --locked -p goatty-app -p goatty-daemon
#   # static goattyds for remote hosts in remote/goattyd-<arch>-linux (see release.yml)
#   scripts/package.sh
set -euo pipefail

cd "$(dirname -- "$0")/.."
out=${1:-dist}
bin=target/dist
version=$(scripts/version.sh)
machine=$(uname -m)
case $machine in
    x86_64) arch=amd64 ;;
    aarch64) arch=arm64 ;;
    *) echo "unsupported architecture: $machine" >&2; exit 1 ;;
esac

for b in goatty goattyd; do
    [ -x "$bin/$b" ] || { echo "missing $bin/$b; build with: cargo build --profile dist --locked -p goatty-app -p goatty-daemon" >&2; exit 1; }
    got=$("$bin/$b" --version)
    [ "$got" = "$b $version" ] || { echo "$bin/$b --version says '$got', expected '$b $version'" >&2; exit 1; }
done
remotes=(remote/goattyd-*-linux)
[ -f "${remotes[0]}" ] || { echo "missing remote/goattyd-<arch>-linux (static goattyds for remote hosts)" >&2; exit 1; }
for r in "${remotes[@]}"; do
    file "$r" | grep -Eq 'static(ally|-pie) linked' || { echo "$r isn't statically linked" >&2; exit 1; }
done

mkdir -p "$out" "$bin/completions"
for b in goatty goattyd; do
    "$bin/$b" completions bash >"$bin/completions/$b.bash"
    "$bin/$b" completions zsh >"$bin/completions/_$b"
    "$bin/$b" completions fish >"$bin/completions/$b.fish"
done

# Release archive: what scripts/install.sh expects next to it, plus the
# launcher and completions that packages built from it (packaging/aur) install.
name="goatty-$version-$machine-linux"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/$name/remote" "$stage/$name/completions"
install -m 755 "$bin/goatty" "$bin/goattyd" scripts/install.sh "$stage/$name/"
install -m 644 README.md LICENSE-MIT LICENSE-APACHE crates/goatty-app/assets/brand/goatty.svg \
    packaging/linux/dev.goatty.goatty.desktop "$stage/$name/"
install -m 755 "${remotes[@]}" "$stage/$name/remote/"
install -m 644 "$bin"/completions/* "$stage/$name/completions/"
tar -C "$stage" -czf "$out/$name.tar.gz" --owner=0 --group=0 "$name"
echo "$out/$name.tar.gz"

if ! command -v nfpm >/dev/null; then
    echo "nfpm isn't installed: skipping the .deb, .rpm and Arch packages" >&2
    exit 0
fi
for packager in deb rpm archlinux; do
    VERSION=$version ARCH=$arch nfpm package --config packaging/nfpm.yaml --packager "$packager" --target "$out/"
done

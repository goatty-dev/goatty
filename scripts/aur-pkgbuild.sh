#!/usr/bin/env bash
# Write the AUR goatty-bin PKGBUILD for a release archive into OUT_DIR:
# packaging/aur/PKGBUILD with the archive's version and checksum.
# With --local, also copy the archive next to it and use it as the source,
# so makepkg can test the package before the release exists (release.yml).
# Usage: scripts/aur-pkgbuild.sh OUT_DIR ARCHIVE [--local]
set -euo pipefail

[ $# -ge 2 ] || { echo "usage: $0 OUT_DIR goatty-<version>-x86_64-linux.tar.gz [--local]" >&2; exit 2; }
out=$1 archive=$2 local=${3:-}
root=$(cd "$(dirname -- "$0")/.." && pwd)

file=$(basename -- "$archive")
version=${file#goatty-}
version=${version%-x86_64-linux.tar.gz}
[ "$version" != "$file" ] || { echo "not a goatty x86_64 release archive: $file" >&2; exit 1; }
[[ $version != *-* ]] || { echo "$version is a pre-release; goatty-bin only follows stable releases" >&2; exit 1; }
sum=$(sha256sum -- "$archive" | cut -d' ' -f1)

mkdir -p "$out"
sed -e "s/^pkgver=.*/pkgver=$version/" \
    -e "s/^pkgrel=.*/pkgrel=1/" \
    -e "s/^sha256sums=.*/sha256sums=('$sum')/" \
    "$root/packaging/aur/PKGBUILD" >"$out/PKGBUILD"
if [ "$local" = --local ]; then
    cp -- "$archive" "$out/"
    sed -i -e "s|^source=.*|source=(\"$file\")|" "$out/PKGBUILD"
fi
grep -q "^pkgver=$version\$" "$out/PKGBUILD" && grep -q "^sha256sums=('$sum')\$" "$out/PKGBUILD" ||
    { echo "failed to fill in $out/PKGBUILD" >&2; exit 1; }
echo "$out/PKGBUILD: goatty-bin $version"

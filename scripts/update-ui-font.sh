#!/usr/bin/env bash
# Fetch the bundled Inter (goatty's UI font) into crates/goatty-app/assets/fonts.
# Pinned release and checksum: bump both on purpose, then rebuild and check
# the font test (`cargo test -p goatty-app fonts`).
#
# Static faces, not the variable font: GPUI's Linux text system renders a
# variable font at weight 400 whatever weight is asked for (zed#64783).
set -euo pipefail

VERSION=4.1
SHA256=9883fdd4a49d4fb66bd8177ba6625ef9a64aa45899767dde3d36aa425756b11e
# The weights goatty's UI uses: NORMAL, MEDIUM, SEMIBOLD, BOLD.
FACES=(Regular Medium SemiBold Bold)

root=$(cd "$(dirname "$0")/.." && pwd)
dest="$root/crates/goatty-app/assets/fonts"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

curl -fsSL -o "$tmp/Inter.zip" "https://github.com/rsms/inter/releases/download/v$VERSION/Inter-$VERSION.zip"
# sha256sum (coreutils) or shasum (macOS, perl).
if command -v sha256sum >/dev/null; then sha256=(sha256sum); else sha256=(shasum -a 256); fi
echo "$SHA256  $tmp/Inter.zip" | "${sha256[@]}" -c -
files=(LICENSE.txt)
for face in "${FACES[@]}"; do files+=("extras/ttf/Inter-$face.ttf"); done
unzip -q -j -o "$tmp/Inter.zip" "${files[@]}" -d "$tmp/out"
mv "$tmp/out/LICENSE.txt" "$dest/Inter-OFL.txt"
for face in "${FACES[@]}"; do mv "$tmp/out/Inter-$face.ttf" "$dest/"; done
echo "updated $dest to Inter $VERSION"

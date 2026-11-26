#!/bin/sh
# Download and install goatty from a GitHub release, without root:
#
#   curl -fsSL https://<website>/install.sh | sh
#   curl -fsSL https://<website>/install.sh | sh -s -- --version 0.4.0
#
# (or straight from GitHub: https://raw.githubusercontent.com/goatty-dev/goatty/main/scripts/get-goatty.sh)
#
# It picks the release archive for this OS and CPU, checks it against the
# release's SHA256SUMS and runs the archive's own install.sh, which installs
# into ~/.local (docs/releasing.md, "What gets installed"). Running it again
# upgrades. The website's /install.sh redirects to this file on main
# (goatty-website public/_redirects), so keep it working for every published
# release. scripts/get-goatty.ps1 is the Windows counterpart.
#
# Everything runs from main() at the end, so a download cut short runs nothing.
set -eu

repo=goatty-dev/goatty
# The oldest glibc the Linux builds run on (release.yml builds on ubuntu-22.04).
min_glibc=2.35

say() { printf 'goatty: %s\n' "$*" >&2; }
die() { say "$*"; exit 1; }
has() { command -v "$1" >/dev/null 2>&1; }

usage() {
    cat <<EOF
Install goatty into ~/.local (no root needed).

Usage: install.sh [--version VERSION]

  --version VERSION  install this release (e.g. 0.4.0) instead of the latest
  -h, --help         show this help
EOF
}

# fetch URL FILE [progress]: download, with a progress bar for big files when
# stderr is a terminal. Retries, since curl's --retry skips dropped connections.
fetch() {
    for attempt in 1 2 3; do
        if has curl; then
            if [ $# -ge 3 ] && [ -t 2 ]; then show=--progress-bar; else show=-sS; fi
            curl --proto '=https' --tlsv1.2 -fL "$show" -o "$2" "$1" && return 0
        elif has wget; then
            if [ $# -ge 3 ] && [ -t 2 ]; then show=--show-progress; else show=; fi
            # shellcheck disable=SC2086 # $show is empty or one flag
            wget --https-only -q $show -O "$2" "$1" && return 0
        else
            die "needs curl or wget to download goatty"
        fi
        [ "$attempt" = 3 ] || { say "download failed, retrying"; sleep 2; }
    done
    return 1
}

# sha256 FILE: print its SHA-256.
sha256() {
    if has sha256sum; then
        sha256sum "$1" | cut -d' ' -f1
    elif has shasum; then
        shasum -a 256 "$1" | cut -d' ' -f1
    elif has openssl; then
        openssl dgst -sha256 -r "$1" | cut -d' ' -f1
    else
        die "needs sha256sum, shasum or openssl to verify the download"
    fi
}

# version_lt A B: true if version A is older than B (numeric, dot-separated).
version_lt() {
    [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n | head -n1)" = "$1" ]
}

check_glibc() {
    glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null | cut -d' ' -f2) || glibc=
    if [ -z "$glibc" ]; then
        die "goatty's Linux build needs glibc $min_glibc or newer, and this system doesn't seem to use glibc (musl, e.g. Alpine?)"
    fi
    if version_lt "$glibc" "$min_glibc"; then
        die "goatty's Linux build needs glibc $min_glibc or newer; this system has $glibc"
    fi
}

# The distro's package names for the libraries goatty needs (packaging/nfpm.yaml).
library_hint() {
    if has apt-get; then
        echo "sudo apt install libxkbcommon-x11-0 libwayland-egl1 libvulkan1 mesa-vulkan-drivers"
    elif has dnf; then
        echo "sudo dnf install libxkbcommon-x11 libwayland-egl vulkan-loader mesa-vulkan-drivers"
    elif has pacman; then
        echo "sudo pacman -S libxkbcommon-x11 wayland vulkan-icd-loader vulkan-mesa-layers (or install the AUR package goatty-bin, which pulls them in)"
    elif has zypper; then
        echo "sudo zypper install libxkbcommon-x11-0 libwayland-egl1 libvulkan1 Mesa-vulkan-device-select"
    fi
}

# Libraries goatty links, plus the ones GPUI opens at runtime.
# (POSIX sh has no local variables: the names here must not clash with main's.)
check_libraries() {
    missing=
    if has ldd; then
        missing=$(ldd "$1" 2>/dev/null | awk '/not found/ { print $1 }' | tr '\n' ' ')
    fi
    ldconfig=$(command -v ldconfig 2>/dev/null || echo /sbin/ldconfig)
    if [ -x "$ldconfig" ]; then
        libs=$("$ldconfig" -p 2>/dev/null) || libs=
        if [ -n "$libs" ]; then
            for lib in libvulkan.so.1 libwayland-client.so.0 libwayland-egl.so.1; do
                printf '%s\n' "$libs" | grep -Fq "$lib" || missing="$missing$lib "
            done
        fi
    fi
    [ -n "$missing" ] || return 0
    say "missing libraries goatty needs: $missing"
    hint=$(library_hint)
    [ -z "$hint" ] || say "install them with: $hint"
}

main() {
    version=
    while [ $# -gt 0 ]; do
        case $1 in
            --version) [ $# -ge 2 ] || die "--version needs a value"; version=$2; shift 2 ;;
            --version=*) version=${1#--version=}; shift ;;
            -h | --help) usage; exit 0 ;;
            *) usage >&2; die "unknown option: $1" ;;
        esac
    done
    version=${version#v}

    case $(uname -s) in
        Linux) os=linux ;;
        Darwin) os=macos ;;
        MINGW* | MSYS* | CYGWIN*)
            die "on Windows, run this in PowerShell instead: irm https://raw.githubusercontent.com/$repo/main/scripts/get-goatty.ps1 | iex" ;;
        *) die "goatty has no build for $(uname -s)" ;;
    esac
    case $(uname -m) in
        x86_64 | amd64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) die "goatty has no build for $(uname -m) CPUs" ;;
    esac
    # Rosetta: a shell translated to x86_64 on an Apple Silicon Mac.
    if [ "$os" = macos ] && [ "$arch" = x86_64 ] &&
        [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
        arch=aarch64
    fi
    target=$arch-$os

    if [ -n "$version" ]; then
        url=https://github.com/$repo/releases/download/v$version
    else
        url=https://github.com/$repo/releases/latest/download
    fi

    tmp=$(mktemp -d 2>/dev/null || mktemp -d -t goatty)
    trap 'rm -rf "$tmp"' EXIT
    trap 'exit 130' INT TERM

    if [ -n "$version" ]; then say "looking up goatty $version"; else say "looking up the latest goatty release"; fi
    fetch "$url/SHA256SUMS" "$tmp/SHA256SUMS" ||
        die "couldn't download $url/SHA256SUMS${version:+ (is $version a goatty release?)}"

    # The archive's name carries the version: goatty-<version>-<target>.tar.gz.
    line=$(grep -E "  goatty-[^ ]+-$target\.tar\.gz\$" "$tmp/SHA256SUMS" | head -n1) || line=
    if [ -z "$line" ]; then
        builds=$(sed -n -E 's/.*  goatty-.*-([a-z0-9_]+-[a-z]+)\.(tar\.gz|zip|msi)$/\1/p' "$tmp/SHA256SUMS" | sort -u | tr '\n' ' ')
        die "this release has no goatty for $target yet (it has: ${builds:-none}). See https://github.com/$repo/releases"
    fi
    want=${line%% *}
    archive=${line##* }
    name=${archive%.tar.gz}

    [ "$os" != linux ] || check_glibc

    say "downloading $archive"
    fetch "$url/$archive" "$tmp/$archive" progress || die "couldn't download $url/$archive"
    got=$(sha256 "$tmp/$archive")
    [ "$got" = "$want" ] || die "$archive doesn't match SHA256SUMS (got $got, expected $want); try again"

    mkdir "$tmp/x"
    tar -xzf "$tmp/$archive" -C "$tmp/x"
    [ -f "$tmp/x/$name/install.sh" ] || die "$archive has no install.sh"

    if [ "$(id -u)" = 0 ]; then
        say "running as root: installing for root into $HOME/.local. To install for yourself, run this without sudo."
    fi
    # Running goatty and goattyd keep working: install(1) replaces the files.
    sh "$tmp/x/$name/install.sh"

    bin=$HOME/.local/bin
    [ "$os" != linux ] || check_libraries "$bin/goatty"

    for other in /usr/bin/goatty /usr/local/bin/goatty; do
        if [ -x "$other" ]; then
            say "$other is installed too (a package?). Whichever comes first on PATH runs; remove one of them."
        fi
    done
    if has pgrep && pgrep -x goattyd >/dev/null 2>&1; then
        say "goattyd is running. To switch it to this version, run: $bin/goattyd restart (your sessions keep running)"
    fi
    say "installed $("$bin/goatty" --version 2>/dev/null || echo "$name")"
}

main "$@"

#!/bin/sh
# Print the workspace version (`version` under [workspace.package]) of a
# Cargo.toml: the given file, `-` for stdin, or the repository's by default.
# Release tags are `v` plus this version (docs/releasing.md).
set -eu

file=${1:-"$(dirname -- "$0")/../Cargo.toml"}
[ "$file" = - ] && file=/dev/stdin

awk '
    /^\[/ { section = $1; next }
    section == "[workspace.package]" && /^version[[:space:]]*=/ {
        sub(/^version[[:space:]]*=[[:space:]]*"/, "")
        sub(/".*/, "")
        print
        found = 1
        exit
    }
    END { if (!found) { print "no [workspace.package] version found" > "/dev/stderr"; exit 1 } }
' "$file"

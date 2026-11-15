#!/usr/bin/env bash
# Open a pull request that sets the workspace version to VERSION. Merging it
# makes the Release workflow tag vVERSION and publish the release
# (docs/releasing.md). Usage: scripts/bump-version.sh 0.2.0 | 0.3.0-rc.1
set -euo pipefail

usage() {
    echo "usage: $0 VERSION  (semver without the v, e.g. 0.2.0 or 0.3.0-rc.1)" >&2
    exit 2
}
[ $# -eq 1 ] || usage
version=${1#v}
semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z.-]+)?$'
[[ $version =~ $semver ]] || { echo "not a version: $1" >&2; usage; }
tag="v$version"

cd "$(git rev-parse --show-toplevel)"
[ -z "$(git status --porcelain)" ] || { echo "commit or stash your changes first" >&2; exit 1; }
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null; then
    echo "$tag is already released" >&2
    exit 1
fi

git fetch --quiet origin main
current=$(git show origin/main:Cargo.toml | scripts/version.sh -)
[ "$version" != "$current" ] || { echo "main is already at $current" >&2; exit 1; }

start=$(git symbolic-ref --quiet --short HEAD || git rev-parse HEAD)
branch="release/$tag"
git switch --quiet -c "$branch" origin/main
trap 'git switch --quiet "$start"' EXIT

tmp=$(mktemp)
awk -v v="$version" '
    /^\[/ { section = $1 }
    section == "[workspace.package]" && !done && /^version[[:space:]]*=/ { print "version = \"" v "\""; done = 1; next }
    { print }
' Cargo.toml >"$tmp"
cat "$tmp" >Cargo.toml
rm -f "$tmp"
# Only the workspace crates' entries change in the lockfile.
cargo update --workspace --offline --quiet 2>/dev/null || cargo update --workspace --quiet

[ "$(scripts/version.sh)" = "$version" ] || { echo "failed to set the version in Cargo.toml" >&2; exit 1; }
unexpected=$(git diff --name-only | grep -vxE 'Cargo\.(toml|lock)' || true)
[ -z "$unexpected" ] || { echo "unexpected changes: $unexpected" >&2; exit 1; }

kind=release
[[ $version == *-* ]] && kind=pre-release
git commit --quiet -am "Release $tag"
git push --quiet -u origin "$branch"
gh pr create --base main --head "$branch" --title "Release $tag" --body "$(cat <<EOF
Sets the workspace version to $version (was $current).

Merging this makes the Release workflow tag \`$tag\` on the merge commit and publish it as a GitHub $kind: the Linux archive, .deb, .rpm and Arch packages and the static goattyds. The release notes list the pull requests merged since the previous release.
EOF
)"

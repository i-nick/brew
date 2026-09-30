#!/usr/bin/env bash
# Print the next release version.
#
# Usage: next-version.sh <patch|minor|major>
#
# The latest v* tag is bumped by the given level. If brew_cli/Cargo.toml was
# set by hand to a higher version that has no tag yet, that version wins.
set -euo pipefail

level="${1:-patch}"

latest=$(git tag -l 'v[0-9]*' --sort=-v:refname | head -1)
latest="${latest#v}"
: "${latest:=0.0.0}"

IFS=. read -r major minor patch <<< "$latest"
case "$level" in
major) next="$((major + 1)).0.0" ;;
minor) next="$major.$((minor + 1)).0" ;;
patch) next="$major.$minor.$((patch + 1))" ;;
*)
    echo "unknown bump level: $level (expected patch, minor or major)" >&2
    exit 1
    ;;
esac

manual=$(sed -n 's/^version = "\(.*\)"/\1/p' brew_cli/Cargo.toml | head -1)
if [[ -n "$manual" ]] && ! git rev-parse -q --verify "refs/tags/v$manual" >/dev/null; then
    highest=$(printf '%s\n%s\n' "$next" "$manual" | sort -V | tail -1)
    if [[ "$highest" == "$manual" ]]; then
        next="$manual"
    fi
fi

echo "$next"

#!/usr/bin/env bash
# Cut a release: scripts/release.sh 0.2.0
#
# Sets the version in Cargo.toml, checks CHANGELOG.md has a section dated today, commits,
# creates an annotated tag v0.2.0 and pushes both. The tag push starts release.yml, which
# builds and publishes the AppImage.
set -euo pipefail

version=${1:?usage: scripts/release.sh X.Y.Z}
version=${version#v}
tag="v$version"
cd "$(dirname "$0")/.."

[[ $version =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.]+)?$ ]] || { echo "not a SemVer version: $version" >&2; exit 1; }
[ -z "$(git status --porcelain)" ] || { echo "working tree is not clean" >&2; exit 1; }
branch=$(git rev-parse --abbrev-ref HEAD)
[ "$branch" = master ] || [ "$branch" = main ] || { echo "release from master/main, not $branch" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$tag" >/dev/null && { echo "$tag already exists" >&2; exit 1; }
today=$(date +%F)
grep -q "^## $today" CHANGELOG.md || { echo "CHANGELOG.md has no '## $today' section; add the changes first" >&2; exit 1; }

sed -i "0,/^version = \".*\"/s//version = \"$version\"/" Cargo.toml
cargo update --workspace --offline >/dev/null 2>&1 || cargo update --workspace >/dev/null
cargo test --locked --quiet

git add Cargo.toml Cargo.lock
git diff --cached --quiet || git commit -m "Release $tag"
git tag -a "$tag" -m "Rust Drac $tag"
git push origin "$branch" "$tag"
echo "Pushed $tag; the Release workflow builds and publishes it."

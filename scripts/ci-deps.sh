#!/usr/bin/env bash
# Install build dependencies on Debian/Ubuntu (CI job image node:22-bookworm, or a local
# container): the GTK 3 / WebKitGTK 4.1 / libsoup 3 headers and a Rust toolchain.
set -euo pipefail

sudo=""
[ "$(id -u)" -eq 0 ] || sudo=sudo
export DEBIAN_FRONTEND=noninteractive
# curl, git and ca-certificates come with the job image. Don't reinstall ca-certificates:
# its post-install script fails on runners that inject a corporate CA into the bundle.
for tool in curl git; do command -v "$tool" >/dev/null || missing="${missing:-} $tool"; done
$sudo apt-get update -qq
$sudo apt-get install -y -q --no-install-recommends \
  build-essential pkg-config file patchelf \
  libgtk-3-dev libwebkit2gtk-4.1-dev libsoup-3.0-dev libdbus-1-dev \
  glib-networking librsvg2-common shared-mime-info ${missing:-}

if ! command -v cargo >/dev/null 2>&1; then
  curl -fsSL --retry 3 https://sh.rustup.rs | sh -s -- -y --profile minimal --component rustfmt,clippy
fi
# shellcheck disable=SC1091
. "$HOME/.cargo/env"
rustup toolchain install stable --profile minimal --component rustfmt,clippy >/dev/null
# Later CI steps find cargo through GITHUB_PATH.
echo "$HOME/.cargo/bin" >> "${GITHUB_PATH:-/dev/null}"
rustc --version
pkg-config --modversion webkit2gtk-4.1 gtk+-3.0

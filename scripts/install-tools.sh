#!/usr/bin/env bash
# Download pinned, checksum-verified tools into ./bin (or $BIN_DIR).
#
#   scripts/install-tools.sh gitleaks betterleaks cargo-deny linuxdeploy appimagetool
#
# Bump a tool by changing its version, URL and sha256 together.
set -euo pipefail

BIN_DIR=${BIN_DIR:-"$(cd "$(dirname "$0")/.." && pwd)/bin"}
mkdir -p "$BIN_DIR"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

fetch() { # name url sha256
  local name=$1 url=$2 sum=$3 file="$tmp/$1.download"
  echo "==> $name" >&2
  # GitHub release downloads sometimes return 502/504 for a minute or two;
  # back off long enough to ride that out, and say so plainly if it doesn't.
  curl -fsSL --retry 6 --retry-delay 10 --retry-connrefused -o "$file" "$url" \
    || { echo "download failed for $name: $url" >&2; exit 1; }
  echo "$sum  $file" | sha256sum -c --quiet - || { echo "checksum mismatch for $name" >&2; exit 1; }
  printf '%s' "$file"
}

untar() { # name archive member
  tar -xzf "$2" -C "$tmp" "$3"
  install -m 0755 "$tmp/$3" "$BIN_DIR/$1"
}

for tool in "$@"; do
  case $tool in
    gitleaks)
      f=$(fetch gitleaks https://github.com/gitleaks/gitleaks/releases/download/v8.30.1/gitleaks_8.30.1_linux_x64.tar.gz \
        551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb)
      untar gitleaks "$f" gitleaks ;;
    betterleaks)
      f=$(fetch betterleaks https://github.com/betterleaks/betterleaks/releases/download/v1.9.0/betterleaks_1.9.0_linux_x64.tar.gz \
        f8b185a39ffcece2a1ca82bf3a4e7435cd81963ffd16b7a9128daf75f35f6de7)
      untar betterleaks "$f" betterleaks ;;
    cargo-deny)
      f=$(fetch cargo-deny https://github.com/EmbarkStudios/cargo-deny/releases/download/0.20.2/cargo-deny-0.20.2-x86_64-unknown-linux-musl.tar.gz \
        9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f)
      untar cargo-deny "$f" cargo-deny-0.20.2-x86_64-unknown-linux-musl/cargo-deny ;;
    linuxdeploy)
      f=$(fetch linuxdeploy https://github.com/linuxdeploy/linuxdeploy/releases/download/1-alpha-20251107-1/linuxdeploy-x86_64.AppImage \
        c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d)
      install -m 0755 "$f" "$BIN_DIR/linuxdeploy"
      f=$(fetch linuxdeploy-plugin-gtk https://raw.githubusercontent.com/linuxdeploy/linuxdeploy-plugin-gtk/7a3fbc31a9e5075073ff8790f26effbac5f84453/linuxdeploy-plugin-gtk.sh \
        b0f4cbc684a0103a9651f0955b635eaea0096b3a66c0f5a2c2aa337960375171)
      install -m 0755 "$f" "$BIN_DIR/linuxdeploy-plugin-gtk.sh" ;;
    appimagetool)
      f=$(fetch appimagetool https://github.com/AppImage/appimagetool/releases/download/1.9.1/appimagetool-x86_64.AppImage \
        ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0)
      install -m 0755 "$f" "$BIN_DIR/appimagetool" ;;
    *)
      echo "unknown tool: $tool" >&2; exit 2 ;;
  esac
done

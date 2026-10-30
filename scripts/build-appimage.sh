#!/usr/bin/env bash
# Build the release artefacts into dist/:
#   rustdrac-<version>-x86_64.AppImage        self-contained (bundles GTK 3 and WebKitGTK)
#   rustdrac-<version>-x86_64-linux.tar.gz    binary only; needs webkit2gtk-4.1 installed
#   SHA256SUMS
#
# Needs: Rust, the -dev packages listed in the README, and the tools from
# `scripts/install-tools.sh linuxdeploy appimagetool`. Build on the oldest distro you want
# to support: the AppImage runs on systems with the same or newer glibc.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
bin="$root/bin"
version=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}
version=${version#v}
arch=x86_64
appdir="$root/build/AppDir"
dist="$root/dist"
export APPIMAGE_EXTRACT_AND_RUN=1 # the tools are AppImages; CI containers have no FUSE

echo "==> Building rustdrac $version"
cargo build --release --locked
rm -rf "$appdir" && mkdir -p "$appdir" "$dist"

install -Dm755 target/release/rustdrac "$appdir/usr/bin/rustdrac"
install -Dm644 packaging/rustdrac.desktop "$appdir/usr/share/applications/rustdrac.desktop"
install -Dm644 packaging/rustdrac.svg "$appdir/usr/share/icons/hicolor/scalable/apps/rustdrac.svg"
for s in 16 32 48 64 128 256 512; do
  install -Dm644 "packaging/icons/rustdrac-$s.png" "$appdir/usr/share/icons/hicolor/${s}x${s}/apps/rustdrac.png"
done

# WebKitGTK runs every page in helper processes (WebKitWebProcess, WebKitNetworkProcess)
# that it finds via a path compiled into libwebkit2gtk. Ship them at the same path inside
# the AppDir, and below patch that path to a relative one; AppRun then starts in usr/.
wk_libdir=$(pkg-config --variable=libdir webkit2gtk-4.1)
wk_helpers="$wk_libdir/webkit2gtk-4.1"
case $wk_helpers in
  /usr/*) wk_rel=${wk_helpers#/usr/} ;;
  *) echo "unexpected WebKit helper path $wk_helpers (expected under /usr)" >&2; exit 1 ;;
esac
mkdir -p "$appdir/usr/$wk_rel"
cp -a "$wk_helpers/." "$appdir/usr/$wk_rel/"
rm -f "$appdir/usr/$wk_rel/MiniBrowser" # WebKit's demo browser

# gdk-pixbuf (icons) and GIO identify file types with the shared-mime-info database; ship
# it so images load even on a host without it (AppRun puts usr/share first in XDG_DATA_DIRS).
mkdir -p "$appdir/usr/share"
cp -a /usr/share/mime "$appdir/usr/share/mime"

# HTTPS in WebKit (and GIO) comes from the glib-networking GnuTLS module.
gio_modules=$(pkg-config --variable=giomoduledir gio-2.0)
install -Dm755 "$gio_modules/libgiognutls.so" "$appdir/usr/lib/gio/modules/libgiognutls.so"

mkdir -p "$appdir/apprun-hooks"
cat > "$appdir/apprun-hooks/rustdrac-webkit.sh" <<'EOF'
# WebKit helpers are found relative to usr/ (see build-appimage.sh).
cd "$APPDIR/usr" || exit 1
export GIO_MODULE_DIR="$APPDIR/usr/lib/gio/modules"
# The DMA-BUF renderer needs the host's GPU stack to match the bundled libraries; the
# console page renders fine without it.
export WEBKIT_DISABLE_DMABUF_RENDERER=1
# The GTK plugin forces GTK_THEME=Adwaita:light|dark from the desktop colour scheme, which
# overrides the app's own theme choice (dark by default, see --theme); let the app decide.
unset GTK_THEME
EOF

echo "==> Deploying libraries"
extra=()
while IFS= read -r -d '' f; do
  if file -b "$f" | grep -q 'ELF.*executable'; then extra+=(--executable "$f"); else extra+=(--library "$f"); fi
done < <(find "$appdir/usr/$wk_rel" -type f \( -perm -u+x -o -name '*.so' \) -print0)

PATH="$bin:$PATH" DEPLOY_GTK_VERSION=3 LINUXDEPLOY_OUTPUT_VERSION="$version" \
  "$bin/linuxdeploy" --appdir "$appdir" \
  --executable "$appdir/usr/bin/rustdrac" \
  --desktop-file "$appdir/usr/share/applications/rustdrac.desktop" \
  --icon-file "$appdir/usr/share/icons/hicolor/256x256/apps/rustdrac.png" \
  --library "$appdir/usr/lib/gio/modules/libgiognutls.so" \
  "${extra[@]}" \
  --plugin gtk

# linuxdeploy also copies the helpers to usr/bin and gives the in-place ones a RUNPATH of
# $ORIGIN; WebKit launches the in-place ones, so point them at usr/lib and drop the copies.
for f in "$appdir"/usr/bin/WebKit* "$appdir/usr/lib/libwebkit2gtkinjectedbundle.so"; do
  rm -f "$f"
done
while IFS= read -r -d '' f; do
  rel=$(realpath --relative-to="$(dirname "$f")" "$appdir/usr/lib")
  patchelf --set-rpath "\$ORIGIN/$rel" "$f"
done < <(find "$appdir/usr/$wk_rel" -type f \( -perm -u+x -o -name '*.so' \) -print0)

# Point libwebkit2gtk at the bundled helpers: "/usr/lib/…" -> "././/lib/…" (same length).
lib=$(find "$appdir/usr/lib" -maxdepth 1 -name 'libwebkit2gtk-4.1.so*' -type f | head -n1)
[ -n "$lib" ] || { echo "libwebkit2gtk-4.1 was not bundled" >&2; exit 1; }
grep -q "$wk_helpers" "$lib" || { echo "helper path $wk_helpers not found in $lib" >&2; exit 1; }
sed -i "s|$wk_helpers|././/$wk_rel|g" "$lib"

echo "==> Packing AppImage"
appimage="$dist/rustdrac-$version-$arch.AppImage"
ARCH=$arch VERSION="$version" "$bin/appimagetool" --no-appstream "$appdir" "$appimage"

echo "==> Packing tarball"
stage="$root/build/rustdrac-$version-$arch-linux"
rm -rf "$stage" && mkdir -p "$stage"
install -m755 target/release/rustdrac "$stage/"
cp -r packaging README.md LICENSE CHANGELOG.md "$stage/"
tar -C "$root/build" -czf "$dist/rustdrac-$version-$arch-linux.tar.gz" "$(basename "$stage")"

(cd "$dist" && sha256sum "rustdrac-$version-$arch.AppImage" "rustdrac-$version-$arch-linux.tar.gz" > SHA256SUMS)
echo "==> Done"
ls -la "$dist"

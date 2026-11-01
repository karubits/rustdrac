#!/usr/bin/env bash
# Build the Windows release artefacts into dist/ (run from an MSYS2 MINGW64 shell):
#   rustdrac-<version>-x86_64-setup.exe      per-user installer (Inno Setup)
#   rustdrac-<version>-x86_64-windows.zip    portable: unzip anywhere, run rustdrac.exe
#   SHA256SUMS.windows
#
# Needs the MSYS2 packages from the README ("Building on Windows") and Inno Setup 6
# (ISCC.exe; set ISCC to its path if it is not in one of the usual places).
#
# The bundle is a GTK "prefix": rustdrac.exe and every DLL it needs sit in the root, and
# GTK, GLib and gdk-pixbuf find etc/, lib/ and share/ next to them.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
[ "${MSYSTEM:-}" = MINGW64 ] || { echo "run this from an MSYS2 MINGW64 shell" >&2; exit 1; }
version=${VERSION:-$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)}
version=${version#v}
stage="$root/build/windows"
app="$stage/rustdrac-$version"
dist="$root/dist"
mingw=$(cygpath -u "$MINGW_PREFIX")

echo "==> Building rustdrac $version"
cargo build --release --locked
rm -rf "$stage" && mkdir -p "$app" "$dist"
cp target/release/rustdrac.exe "$app/"
cp LICENSE "$app/LICENSE.txt"

# WebView2Loader.dll: the GNU toolchain links its import library, so the DLL itself must
# ship next to the exe (the webview2-com-sys build script unpacks it).
loader=$(find target/release/build -path '*/out/x64/WebView2Loader.dll' | head -n1)
[ -n "$loader" ] || { echo "WebView2Loader.dll not found under target/" >&2; exit 1; }
cp "$loader" "$app/"

# gdk-pixbuf loaders: PNG (the embedded app icon) and SVG (the Adwaita symbolic icons).
# gdk-pixbuf-query-loaders writes paths relative to the prefix when run inside it.
loaders=lib/gdk-pixbuf-2.0/2.10.0/loaders
mkdir -p "$app/$loaders"
cp "$mingw/$loaders/libpixbufloader-png.dll" "$mingw/$loaders/pixbufloader_svg.dll" "$app/$loaders/"
(cd "$mingw/$loaders" && gdk-pixbuf-query-loaders libpixbufloader-png.dll pixbufloader_svg.dll) \
  > "$app/lib/gdk-pixbuf-2.0/2.10.0/loaders.cache"

# Every MinGW DLL the exe and the loaders pull in, transitively.
echo "==> Collecting DLLs"
ntldd -R "$app/rustdrac.exe" "$app/$loaders"/*.dll \
  | awk '/=>/ { print $3 }' | grep -i "mingw64" | sort -u \
  | while IFS= read -r dll; do cp "$(cygpath -u "$dll")" "$app/"; done
ls "$app"/*.dll | wc -l | xargs echo "    DLLs:"

# GSettings schemas (GTK reads org.gtk.Settings.* at startup), icons, GTK settings.
mkdir -p "$app/share/glib-2.0/schemas" "$app/share/icons/Adwaita" "$app/share/icons/hicolor/scalable/apps"
glib-compile-schemas "$mingw/share/glib-2.0/schemas" --targetdir "$app/share/glib-2.0/schemas"
for d in index.theme 16x16 scalable symbolic; do
  cp -r "$mingw/share/icons/Adwaita/$d" "$app/share/icons/Adwaita/"
done
# Adwaita inherits AdwaitaLegacy; without it GTK looks in hicolor only, which has our icons.
sed -i 's/^Inherits=.*/Inherits=hicolor/' "$app/share/icons/Adwaita/index.theme"
cp "$mingw/share/icons/hicolor/index.theme" "$app/share/icons/hicolor/"
cp packaging/rustdrac.svg "$app/share/icons/hicolor/scalable/apps/rustdrac.svg"
for s in 16 32 48 64 128 256 512; do
  install -Dm644 "packaging/icons/rustdrac-$s.png" "$app/share/icons/hicolor/${s}x${s}/apps/rustdrac.png"
done
gtk-update-icon-cache -q -t -f "$app/share/icons/Adwaita"
gtk-update-icon-cache -q -t -f "$app/share/icons/hicolor"
install -Dm644 packaging/windows/settings.ini "$app/etc/gtk-3.0/settings.ini"

echo "==> Smoke test (without MSYS2 on PATH)"
env PATH="$(cygpath -u "$SYSTEMROOT")/System32:$(cygpath -u "$SYSTEMROOT")" "$app/rustdrac.exe" --version

echo "==> Installer"
iscc=${ISCC:-}
for candidate in "$(cygpath -u "$LOCALAPPDATA")/Programs/Inno Setup 6/ISCC.exe" \
                 "/c/Program Files (x86)/Inno Setup 6/ISCC.exe"; do
  [ -n "$iscc" ] || [ ! -x "$candidate" ] || iscc=$candidate
done
[ -n "$iscc" ] || { echo "Inno Setup not found; set ISCC=/path/to/ISCC.exe" >&2; exit 1; }
numeric=$(echo "$version" | sed 's/[^0-9.].*//')
# MSYS2 would turn the /D... switches into paths; hand the arguments over untouched.
MSYS2_ARG_CONV_EXCL='*' "$iscc" /Q "/DAppVersion=$version" "/DNumericVersion=$numeric.0" \
  "/DAppDir=$(cygpath -w "$app")" "/O$(cygpath -w "$dist")" "/Frustdrac-$version-x86_64-setup" \
  "$(cygpath -w "$root/packaging/windows/rustdrac.iss")"

echo "==> Portable zip"
rm -f "$dist/rustdrac-$version-x86_64-windows.zip"
(cd "$stage" && zip -qr "$dist/rustdrac-$version-x86_64-windows.zip" "rustdrac-$version")

(cd "$dist" && sha256sum "rustdrac-$version-x86_64-setup.exe" "rustdrac-$version-x86_64-windows.zip" > SHA256SUMS.windows)
echo "==> Done:"
ls -la "$dist"

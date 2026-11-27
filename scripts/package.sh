#!/usr/bin/env bash
# Builds a portable Draisine folder (+ archive) with the matching rclone binary next to it.
#
#   scripts/package.sh                      # host platform
#   scripts/package.sh aarch64-apple-darwin # explicit Rust target
#   RCLONE_VERSION=v1.76.0 scripts/package.sh
#
# Output: dist/draisine-<version>-<os>-<arch>/  and  dist/draisine-<os>-<arch>.(tar.gz|zip)
# The archive name has no version, so the landing page can link to
# https://github.com/hekori/draisine/releases/latest/download/<archive>.
# macOS: dist/.../Draisine.app (tray-only app, rclone inside Contents/MacOS).
set -euo pipefail
cd "$(dirname "$0")/.."

RCLONE_VERSION="${RCLONE_VERSION:-v1.76.0}"
TARGET="${1:-$(rustc -vV | sed -n 's/^host: //p')}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"

case "$TARGET" in
  x86_64-unknown-linux-gnu)  OS=linux;   ARCH=x86_64;  RC=linux-amd64 ;;
  aarch64-unknown-linux-gnu) OS=linux;   ARCH=aarch64; RC=linux-arm64 ;;
  aarch64-apple-darwin)      OS=macos;   ARCH=aarch64; RC=osx-arm64 ;;
  x86_64-pc-windows-*)       OS=windows; ARCH=x86_64;  RC=windows-amd64 ;;
  aarch64-pc-windows-*)      OS=windows; ARCH=aarch64; RC=windows-arm64 ;;
  *) echo "unsupported target $TARGET" >&2; exit 1 ;;
esac
EXE=""; [[ $OS == windows ]] && EXE=".exe"

echo "== build $TARGET"
cargo build --release --target "$TARGET"

NAME="draisine-$VERSION-$OS-$ARCH"
OUT="dist/$NAME"
rm -rf "$OUT" && mkdir -p "$OUT"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

echo "== rclone $RCLONE_VERSION ($RC)"
curl -fsSL -o "$TMP/rclone.zip" "https://downloads.rclone.org/$RCLONE_VERSION/rclone-$RCLONE_VERSION-$RC.zip"
mkdir -p "$TMP/rclone"
unzip -q "$TMP/rclone.zip" -d "$TMP/rclone"
RCLONE_BIN="$(find "$TMP/rclone" -type f -name "rclone$EXE" | head -1)"

if [[ $OS == macos ]]; then
  APP="$OUT/Draisine.app/Contents"
  mkdir -p "$APP/MacOS"
  cp "target/$TARGET/release/draisine" "$APP/MacOS/draisine"
  cp "$RCLONE_BIN" "$APP/MacOS/rclone"
  chmod +x "$APP/MacOS/"*
  cat > "$APP/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>Draisine</string>
  <key>CFBundleIdentifier</key><string>com.hekori.draisine</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleExecutable</key><string>draisine</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict></plist>
EOF
else
  cp "target/$TARGET/release/draisine$EXE" "$OUT/"
  cp "$RCLONE_BIN" "$OUT/"
  chmod +x "$OUT/"* 2>/dev/null || true
fi
cp packaging/README.txt "$OUT/"
cp assets/THIRD_PARTY.txt "$OUT/"
cp LICENSE "$OUT/LICENSE.txt"

echo "== archive"
ARCHIVE="draisine-$OS-$ARCH"
cd dist
if [[ $OS == windows ]]; then
  rm -f "$ARCHIVE.zip"
  if command -v 7z >/dev/null; then 7z a -bd "$ARCHIVE.zip" "$NAME" >/dev/null; else zip -qr "$ARCHIVE.zip" "$NAME"; fi
  echo "dist/$ARCHIVE.zip"
else
  tar czf "$ARCHIVE.tar.gz" "$NAME"
  echo "dist/$ARCHIVE.tar.gz"
fi

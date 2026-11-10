#!/bin/sh
# Release build for Apple Silicon macOS, then refresh the artifacts at the repository root.
# Keeping the copy in the script avoids shipping a bundle that is older than src/.
set -eu
IDENTITY="${MOUSEBORDER_SIGN_IDENTITY:-MouseBorder Dev Code Signing}"
command -v cargo >/dev/null 2>&1 || PATH="$HOME/.cargo/bin:$PATH"
cd "$(dirname "$0")"
cargo tauri build --bundles app --no-sign
cd ..
rm -rf MouseBorder.app MouseBorder-Tauri-mac-arm64.zip
cp -R native/target/release/bundle/macos/MouseBorder.app .
# A stable signing identity is what keeps the Accessibility and Input Monitoring grants alive
# across rebuilds; an ad-hoc signature changes its code requirement on every build.
if security find-identity -v -p codesigning 2>/dev/null | grep -q "\"$IDENTITY\""; then
  codesign --force --timestamp=none -s "$IDENTITY" MouseBorder.app
  codesign --verify --verbose=2 MouseBorder.app
else
  echo "warning: signing identity \"$IDENTITY\" not found; shipping ad-hoc, so macOS input permissions will reset after this build" >&2
fi
zip -qry MouseBorder-Tauri-mac-arm64.zip MouseBorder.app
du -sh MouseBorder.app MouseBorder-Tauri-mac-arm64.zip

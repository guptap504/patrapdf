#!/usr/bin/env bash
# Wrap a release build in "PatraPDF.app" and a drag-to-Applications .dmg.
#
#   cargo build --release --target aarch64-apple-darwin
#   packaging/macos/bundle.sh aarch64-apple-darwin dist
#
# The app is ad-hoc signed (no Apple Developer ID), so the first launch needs
# right-click → Open, or System Settings → Privacy & Security → Open Anyway.
set -euo pipefail
target=${1:?usage: bundle.sh <rust-target> [out-dir]}
out=${2:-dist}
root=$(cd "$(dirname "$0")/../.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
case "$target" in
  aarch64-*) arch=arm64 ;;
  x86_64-*) arch=x86_64 ;;
  *) echo "unsupported target $target" >&2; exit 1 ;;
esac

mkdir -p "$out"
stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
app="$stage/PatraPDF.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$root/target/$target/release/patrapdf" "$app/Contents/MacOS/patrapdf"
cp "$root/packaging/macos/AppIcon.icns" "$app/Contents/Resources/AppIcon.icns"
sed "s/@VERSION@/$version/g" "$root/packaging/macos/Info.plist" > "$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist" >/dev/null
codesign --force --sign - --timestamp=none "$app"
ln -s /Applications "$stage/Applications"

dmg="$out/PatraPDF-$version-macos-$arch.dmg"
rm -f "$dmg"
hdiutil create -quiet -volname "PatraPDF" -srcfolder "$stage" -fs HFS+ -format UDZO "$dmg"
echo "$dmg"

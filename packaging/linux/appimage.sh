#!/usr/bin/env bash
# Build an AppImage from `target/release/patrapdf` with linuxdeploy.
#
#   cargo build --release && packaging/linux/appimage.sh dist
set -euo pipefail
out=${1:-dist}
root=$(cd "$(dirname "$0")/../.." && pwd)
version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
id=io.github.guptap504.PatraPDF
tools="$root/target/appimage-tools"
mkdir -p "$tools" "$out"
out=$(cd "$out" && pwd)

# A tagged release of linuxdeploy, checked against its checksum: what it puts
# in the AppImage is signed and installed by the updater like the rest of it.
linuxdeploy_version=1-alpha-20251107-1
linuxdeploy_sha256=c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d
linuxdeploy="$tools/linuxdeploy-$linuxdeploy_version-x86_64.AppImage"
if [ ! -x "$linuxdeploy" ]; then
  curl -fsSL -o "$linuxdeploy.part" \
    "https://github.com/linuxdeploy/linuxdeploy/releases/download/$linuxdeploy_version/linuxdeploy-x86_64.AppImage"
  echo "$linuxdeploy_sha256  $linuxdeploy.part" | sha256sum -c -
  mv "$linuxdeploy.part" "$linuxdeploy"
  chmod +x "$linuxdeploy"
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
appdir="$work/AppDir"
icon_args=()
for size in 16 24 32 48 64 128 256 512; do
  mkdir -p "$work/icons/$size"
  cp "$root/packaging/linux/icons/$size.png" "$work/icons/$size/$id.png"
  icon_args+=(--icon-file "$work/icons/$size/$id.png")
done
mkdir -p "$appdir/usr/share/metainfo"
cp "$root/packaging/linux/$id.metainfo.xml" "$appdir/usr/share/metainfo/$id.appdata.xml"

# CI runners have no FUSE; run the tool from its extracted self.
export APPIMAGE_EXTRACT_AND_RUN=1 ARCH=x86_64 LINUXDEPLOY_OUTPUT_VERSION="$version"
cd "$work"
"$linuxdeploy" --appdir "$appdir" \
  --executable "$root/target/release/patrapdf" \
  --desktop-file "$root/packaging/linux/$id.desktop" \
  "${icon_args[@]}" \
  --output appimage
mv "$work"/PatraPDF-*.AppImage "$out/PatraPDF-$version-linux-x86_64.AppImage"
echo "$out/PatraPDF-$version-linux-x86_64.AppImage"

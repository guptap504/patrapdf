#!/usr/bin/env bash
# Regenerate every icon file from the two SVG sources. Needs resvg
# (`cargo install resvg`), ImageMagick and, for the .icns, macOS's iconutil.
# The outputs are committed, so packaging doesn't need these tools.
set -euo pipefail
cd "$(dirname "$0")"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

# macOS: AppIcon.icns from the icon-grid variant.
set_dir="$tmp/AppIcon.iconset"
mkdir -p "$set_dir"
for size in 16 32 128 256 512; do
  resvg -w "$size" icon-macos.svg "$set_dir/icon_${size}x${size}.png"
  resvg -w $((size * 2)) icon-macos.svg "$set_dir/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$set_dir" -o ../macos/AppIcon.icns

# Linux: hicolor PNGs.
for size in 16 24 32 48 64 128 256 512; do
  resvg -w "$size" icon.svg "../linux/icons/${size}.png"
done

# Windows: multi-resolution .ico.
for size in 16 20 24 32 40 48 64 128 256; do
  resvg -w "$size" icon.svg "$tmp/win-$size.png"
done
magick "$tmp"/win-{16,20,24,32,40,48,64,128,256}.png ../windows/app.ico

# README / About artwork.
resvg -w 256 icon-macos.svg icon-256.png

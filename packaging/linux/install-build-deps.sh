#!/usr/bin/env bash
# System packages needed to build (and test) on Debian/Ubuntu.
set -euo pipefail
sudo=""; [ "$(id -u)" -ne 0 ] && sudo=sudo
$sudo apt-get update
$sudo apt-get install -y --no-install-recommends \
  build-essential pkg-config clang \
  libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-dev libx11-xcb-dev libxcb1-dev \
  libfontconfig1-dev libfreetype6-dev libvulkan-dev libzstd-dev libssl-dev \
  fonts-liberation file desktop-file-utils rpm

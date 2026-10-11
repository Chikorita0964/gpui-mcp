#!/usr/bin/env bash
# The native libraries GPUI and the capture backends build against on Ubuntu, for every CI job
# that builds the workspace. Extra packages a job needs (a display for windowed tests) follow as
# arguments.
set -euo pipefail

sudo apt-get update
sudo apt-get install --no-install-recommends \
  clang libclang-dev pkg-config cmake \
  libfontconfig-dev libwayland-dev libxkbcommon-dev libxkbcommon-x11-dev \
  libxrandr-dev libegl-dev libgbm-dev \
  libx11-xcb-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev \
  libxcb-xfixes0-dev libxcb-randr0-dev libxcb-shm0-dev libxcb-xinput-dev \
  libxcb-composite0-dev libpipewire-0.3-dev libdbus-1-dev libegl1-mesa-dev \
  mesa-vulkan-drivers "$@"

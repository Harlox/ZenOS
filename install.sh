#!/bin/bash

echo "=== ZenOS Install Script ==="

# Mise à jour système
sudo pacman -Syu --noconfirm

# Dépendances
sudo pacman -S --noconfirm \
  git \
  curl \
  base-devel \
  pkgconf \
  wayland \
  libxkbcommon \
  libxcursor \
  libxi \
  libx11 \
  ttf-dejavu \
  xorg-server \
  xorg-xinit \
  openbox \
  xterm

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Cloner ZenOS
git clone https://github.com/Harlox/ZenOS ~/ZenOS
cd ~/ZenOS

# Compiler
cargo build --release

echo "=== ZenOS installé ! ==="
echo "Pour lancer : cd ~/ZenOS && cargo run --release"
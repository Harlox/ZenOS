#!/bin/bash

echo "=== ZenOS Install Script ==="

# Mise à jour système
sudo pacman -Syu --noconfirm

# Dépendances minimales
sudo pacman -S --noconfirm \
  git \
  curl \
  base-devel \
  ttf-dejavu \
  mesa \
  vulkan-icd-loader \
  vulkan-validation-layers

# Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source ~/.cargo/env

# Cloner ZenOS
git clone https://github.com/Harlox/ZenOS ~/ZenOS
cd ~/ZenOS

# Compiler en release
cargo build --release

echo ""
echo "=== ZenOS installé ! ==="
echo "Pour lancer : cd ~/ZenOS && cargo run --release"
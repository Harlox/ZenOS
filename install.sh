#!/bin/bash
set -e

echo "=== ZenOS Install Script ==="

# Mise à jour système
sudo pacman -Syu --noconfirm

# Dépendances minimales + libs winit (Wayland + X11)
sudo pacman -S --needed --noconfirm \
  git \
  curl \
  base-devel \
  pciutils \
  ttf-dejavu \
  mesa \
  vulkan-icd-loader \
  vulkan-tools \
  vulkan-validation-layers \
  wayland \
  libxkbcommon \
  libx11 \
  libxcursor \
  libxrandr \
  libxi

# Driver Vulkan vendor (détection GPU via lspci)
GPU=$(lspci | grep -Ei 'vga|3d|display')
echo "GPU détecté : $GPU"
if echo "$GPU" | grep -qi 'nvidia'; then
  sudo pacman -S --needed --noconfirm nvidia-utils
elif echo "$GPU" | grep -qi 'amd\|ati\|radeon'; then
  sudo pacman -S --needed --noconfirm vulkan-radeon
elif echo "$GPU" | grep -qi 'intel'; then
  sudo pacman -S --needed --noconfirm vulkan-intel
else
  echo "GPU non reconnu — installe le driver Vulkan vendor manuellement"
  echo "(vulkan-radeon / vulkan-intel / nvidia-utils)"
fi

# Rust
if ! command -v cargo >/dev/null 2>&1; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi
source "$HOME/.cargo/env"

# Vérif Vulkan : adapter dispo, sinon ZenOS panic au boot
if ! vulkaninfo 2>/dev/null | grep -q deviceName; then
  echo "ERREUR : aucun device Vulkan détecté."
  echo "Installe le driver vendor (étape ci-dessus) puis relance."
  exit 1
fi

# Cloner ZenOS (skip si déjà présent)
if [ ! -d "$HOME/ZenOS" ]; then
  git clone https://github.com/Harlox/ZenOS "$HOME/ZenOS"
fi
cd "$HOME/ZenOS"

# Compiler en release
cargo build --release

echo ""
echo "=== ZenOS installé ! ==="
echo "Pour lancer : cd ~/ZenOS && cargo run --release"

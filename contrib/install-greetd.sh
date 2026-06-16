#!/bin/bash
# Milestone 7: boot into ZenOS without sudo, via greetd.
# Run from the repo root: contrib/install-greetd.sh
set -e

USER_NAME="${SUDO_USER:-$USER}"

# greetd: minimal login/session daemon that owns the seat session.
sudo pacman -S --needed --noconfirm greetd

# Build + install the binary and session launcher.
cargo build --release
sudo install -Dm755 target/release/zenos      /usr/local/bin/zenos
sudo install -Dm755 contrib/zenos-session     /usr/local/bin/zenos-session

# greetd config (autologin straight into ZenOS). Patch the username to match.
sudo install -Dm644 contrib/greetd-config.toml /etc/greetd/config.toml
sudo sed -i "s/^user = .*/user = \"${USER_NAME}\"/" /etc/greetd/config.toml

sudo systemctl enable greetd

echo
echo "=== ZenOS boot via greetd installed (user: ${USER_NAME}) ==="
echo "Make sure no other display manager is enabled:"
echo "  systemctl disable --now sddm gdm lightdm 2>/dev/null || true"
echo "Test now from a FREE vt (Ctrl+Alt+F2):  sudo systemctl start greetd"
echo "Logs: /tmp/zenos.log   |  Reboot to boot into ZenOS."

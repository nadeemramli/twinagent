#!/usr/bin/env bash
# Build and install the WSL collector as a systemd user service.
set -euo pipefail

cd "$(dirname "$0")/.."

echo "Building twin-collector (release)..."
cargo build --release -p twin-collector

echo "Installing binary to ~/.local/bin..."
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/twin-collector "$HOME/.local/bin/twin-collector"

echo "Installing systemd user unit..."
mkdir -p "$HOME/.config/systemd/user"
install -m 644 packaging/twin-collector.service "$HOME/.config/systemd/user/twin-collector.service"

systemctl --user daemon-reload
systemctl --user enable twin-collector
# restart (not just enable --now) so a reinstall over a running unit actually
# picks up the freshly-built binary instead of leaving the old one running.
systemctl --user restart twin-collector

echo
systemctl --user --no-pager status twin-collector || true
echo
echo "Done. Logs: journalctl --user -u twin-collector -f"
echo "Note: run 'loginctl enable-linger $USER' once so the daemon survives logout."

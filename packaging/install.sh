#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN_DST="${HOME}/.local/bin/asahi-brightness"
UNIT_DST="${HOME}/.config/systemd/user/asahi-brightness.service"
UDEV_DST="/etc/udev/rules.d/99-asahi-brightness.rules"

echo "==> Building release binary"
( cd "$ROOT" && cargo build --release )

echo "==> Installing binary to ${BIN_DST}"
mkdir -p "$(dirname "$BIN_DST")"
install -m 0755 "$ROOT/target/release/asahi-brightness" "$BIN_DST"

echo "==> Installing systemd user unit"
mkdir -p "$(dirname "$UNIT_DST")"
install -m 0644 "$ROOT/packaging/asahi-brightness.service" "$UNIT_DST"
systemctl --user daemon-reload

echo "==> Installing udev rule (sudo required)"
sudo install -m 0644 "$ROOT/packaging/99-asahi-brightness.rules" "$UDEV_DST"
sudo udevadm control --reload-rules
sudo udevadm trigger --subsystem-match=backlight --action=add
sudo udevadm trigger --subsystem-match=leds --action=add

echo "==> Done."
echo "Enable and start with:"
echo "  systemctl --user enable --now asahi-brightness.service"
echo "Tail logs with:"
echo "  journalctl --user -u asahi-brightness -f"

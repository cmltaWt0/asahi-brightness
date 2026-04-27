bin := "asahi-brightness"
dst := env_var('HOME') / ".local/bin" / bin
unit := "asahi-brightness.service"

# Show available recipes.
default:
    @just --list

# Build release binary.
build:
    cargo build --release

# Run unit tests.
test:
    cargo test --release

# Lint with clippy.
lint:
    cargo clippy --release --no-deps -- -D warnings

# Build, install to ~/.local/bin, and restart the user service.
reinstall: build
    install -m 0755 target/release/{{ bin }} {{ dst }}
    systemctl --user restart {{ unit }}

# Full first-time install (binary + systemd unit + udev rule). Asks for sudo.
install-all:
    ./packaging/install.sh

# Tail the daemon's logs.
logs:
    journalctl --user -u {{ unit }} -f

# Show daemon status.
status:
    systemctl --user --no-pager status {{ unit }} || true
    @echo
    {{ dst }} status

# Stop the daemon.
stop:
    systemctl --user stop {{ unit }}

# Start the daemon.
start:
    systemctl --user start {{ unit }}

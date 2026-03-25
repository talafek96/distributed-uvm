#!/usr/bin/env bash
set -euo pipefail

# duvm installer — sets up all prerequisites and installs duvm components.
#
# Usage:
#   sudo ./scripts/install.sh          # Full install (daemon + kernel module)
#   sudo ./scripts/install.sh --no-kmod  # Skip kernel module
#
# Prerequisites installed:
#   - vm.unprivileged_userfaultfd=1 (sysctl, persistent)
#   - Rust toolchain (if not present)
#   - Build tools (gcc, make, kernel headers)
#
# Components installed:
#   - duvm-daemon -> /usr/local/bin/
#   - duvm-ctl -> /usr/local/bin/
#   - duvm-memserver -> /usr/local/bin/
#   - duvm-kmod.ko -> loaded (optional)
#   - systemd service files
#   - Example configuration

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
INSTALL_KMOD=true

for arg in "$@"; do
    case $arg in
        --no-kmod) INSTALL_KMOD=false ;;
        --help|-h) echo "Usage: sudo $0 [--no-kmod]"; exit 0 ;;
    esac
done

echo "=== duvm installer ==="
echo "  Source: $SCRIPT_DIR"
echo "  Kernel module: $INSTALL_KMOD"
echo

# Check root
if [[ $EUID -ne 0 ]]; then
    echo "ERROR: This script must be run as root (sudo)."
    exit 1
fi

# Step 1: System prerequisites
echo "[1/6] Configuring system prerequisites..."

# Enable userfaultfd for non-root users (fallback mode)
if [[ "$(cat /proc/sys/vm/unprivileged_userfaultfd 2>/dev/null)" != "1" ]]; then
    echo "  Setting vm.unprivileged_userfaultfd=1..."
    echo "vm.unprivileged_userfaultfd=1" > /etc/sysctl.d/90-duvm.conf
    sysctl -w vm.unprivileged_userfaultfd=1
else
    echo "  vm.unprivileged_userfaultfd=1 already set"
fi

# Install build dependencies
echo "  Checking build tools..."
if ! command -v gcc &>/dev/null; then
    echo "  Installing gcc..."
    apt-get update -qq && apt-get install -y -qq gcc make
fi

if ! command -v cargo &>/dev/null; then
    echo "  Installing Rust toolchain..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    export PATH="$HOME/.cargo/bin:$PATH"
fi

echo "  OK"

# Step 2: Build
echo "[2/6] Building duvm (release mode)..."
cd "$SCRIPT_DIR"
export PATH="$HOME/.cargo/bin:/root/.cargo/bin:$PATH"
cargo build --release
echo "  OK"

# Step 3: Install binaries
echo "[3/6] Installing binaries..."
install -m 755 target/release/duvm-daemon /usr/local/bin/
install -m 755 target/release/duvm-ctl /usr/local/bin/
install -m 755 target/release/duvm-memserver /usr/local/bin/
echo "  Installed: duvm-daemon, duvm-ctl, duvm-memserver -> /usr/local/bin/"

# Step 4: Install configuration
echo "[4/6] Installing configuration..."
mkdir -p /etc/duvm
if [[ ! -f /etc/duvm/duvm.toml ]]; then
    install -m 644 config/duvm.toml /etc/duvm/duvm.toml
    echo "  Installed: /etc/duvm/duvm.toml"
else
    echo "  /etc/duvm/duvm.toml already exists, skipping"
fi

# Step 5: Install systemd services
echo "[5/6] Installing systemd services..."
install -m 644 config/duvm-daemon.service /etc/systemd/system/
install -m 644 config/duvm-memserver.service /etc/systemd/system/
install -m 644 config/duvm-kmod.service /etc/systemd/system/
systemctl daemon-reload
echo "  Installed: duvm-daemon.service, duvm-memserver.service, duvm-kmod.service"
echo "  Enable on boot: systemctl enable duvm-daemon duvm-memserver"
echo "  Or use: duvm-ctl enable / duvm-ctl disable"

# Step 6: Kernel module (optional)
if $INSTALL_KMOD; then
    echo "[6/6] Building and loading kernel module..."
    if [[ -d /lib/modules/$(uname -r)/build ]]; then
        cd duvm-kmod
        make clean 2>/dev/null || true
        make
        insmod duvm-kmod.ko size_mb=4096
        echo "  Loaded: duvm-kmod.ko (4GB virtual swap device)"
        echo "  Device: /dev/duvm_swap0"
        echo "  To use as swap: mkswap /dev/duvm_swap0 && swapon -p 100 /dev/duvm_swap0"
    else
        echo "  SKIP: kernel headers not found at /lib/modules/$(uname -r)/build"
        echo "  Install with: apt install linux-headers-$(uname -r)"
    fi
else
    echo "[6/6] Skipping kernel module (--no-kmod)"
fi

echo
echo "=== Installation complete ==="
echo
echo "Quick start:"
echo "  sudo duvm-ctl enable             # Load kmod, start services, activate swap"
echo "  duvm-ctl status                   # Check daemon status"
echo "  sudo duvm-ctl disable             # Drain pages, stop services, unload kmod"
echo
echo "Manual control:"
echo "  systemctl start duvm-daemon       # Start daemon only"
echo "  systemctl start duvm-memserver    # Start memserver only"
echo "  duvm-memserver --bind 0.0.0.0:9200  # Start memory server for remote nodes"
echo
echo "For remote memory, start duvm-memserver on each remote node,"
echo "then configure /etc/duvm/duvm.toml with the peer addresses."

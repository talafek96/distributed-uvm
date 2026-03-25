#!/bin/bash
# scripts/setup-kmod-for-testing.sh
#
# Run this with sudo on calc1 to load the kernel module and prepare
# for the daemon to connect. The daemon itself runs without sudo.
#
# Usage:
#   sudo bash scripts/setup-kmod-for-testing.sh
#
# To tear down later:
#   sudo bash scripts/setup-kmod-for-testing.sh --teardown

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KMOD="$PROJECT_ROOT/duvm-kmod/duvm-kmod.ko"

if [[ "${1:-}" == "--teardown" ]]; then
    echo "=== Tearing down duvm ==="
    swapoff /dev/duvm_swap0 2>/dev/null && echo "  swapoff done" || echo "  swap not active"
    rmmod duvm_kmod 2>/dev/null && echo "  rmmod done" || echo "  module not loaded"
    echo "  Done."
    exit 0
fi

echo "=== Setting up duvm kernel module for testing ==="

# Build if needed
if [[ ! -f "$KMOD" ]]; then
    echo "[1/4] Building kernel module..."
    make -C "$PROJECT_ROOT/duvm-kmod" 2>&1 | tail -1
else
    echo "[1/4] Kernel module already built."
fi

# Load
echo "[2/4] Loading kernel module..."
if lsmod | grep -q duvm_kmod; then
    echo "  Already loaded, reloading..."
    swapoff /dev/duvm_swap0 2>/dev/null || true
    rmmod duvm_kmod
fi
insmod "$KMOD" size_mb=4096 ring_entries=256
echo "  Loaded: $(ls -la /dev/duvm_swap0 /dev/duvm_ctl 2>&1)"

# mkswap
echo "[3/4] Creating swap filesystem..."
mkswap /dev/duvm_swap0 > /dev/null
echo "  Done."

# Make /dev/duvm_ctl accessible to the 'tal' user (so daemon doesn't need sudo)
echo "[4/4] Setting permissions..."
chmod 666 /dev/duvm_ctl
echo "  /dev/duvm_ctl is now accessible to all users."

echo ""
echo "=== Ready ==="
echo "  /dev/duvm_swap0 — block device (run 'sudo swapon -p 100 /dev/duvm_swap0' when daemon is connected)"
echo "  /dev/duvm_ctl   — daemon can now connect without sudo"
echo ""
echo "Next steps (no sudo needed):"
echo "  1. Start daemon:   cargo run --release -p duvm-daemon -- --kmod-ctl /dev/duvm_ctl"
echo "  2. Activate swap:  sudo swapon -p 100 /dev/duvm_swap0"
echo ""
echo "To tear down: sudo bash $0 --teardown"

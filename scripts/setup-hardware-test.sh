#!/bin/bash
# scripts/setup-hardware-test.sh — Set up duvm for hardware testing.
#
# Run on the COMPUTE node (the one that swaps pages out).
# The MEMORY node runs duvm-memserver (no sudo needed).
#
# Usage:
#   sudo bash scripts/setup-hardware-test.sh              # Setup
#   sudo bash scripts/setup-hardware-test.sh --teardown   # Teardown
#
# Prerequisites:
#   - Kernel module built: make -C duvm-kmod
#   - Daemon built: cargo build --release -p duvm-daemon

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KMOD="$PROJECT_ROOT/duvm-kmod/duvm-kmod.ko"
SIZE_MB="${DUVM_SIZE_MB:-4096}"

if [[ "${1:-}" == "--teardown" ]]; then
    echo "=== Tearing down duvm hardware test ==="
    swapoff /dev/duvm_swap0 2>/dev/null && echo "  swapoff done" || echo "  swap not active"
    pkill duvm-daemon 2>/dev/null && echo "  daemon killed" || echo "  daemon not running"
    sleep 1
    rmmod duvm_kmod 2>/dev/null && echo "  rmmod done" || echo "  module not loaded"
    echo "  Done."
    exit 0
fi

echo "=== Setting up duvm for hardware testing ==="

# Stop memory guards that interfere with swap testing
if systemctl is-active memory-watchdog &>/dev/null; then
    echo "[0/4] Stopping memory-watchdog (interferes with swap testing)..."
    systemctl stop memory-watchdog
    echo "  Stopped. Remember to restart after testing: systemctl start memory-watchdog"
fi
if systemctl is-active earlyoom &>/dev/null; then
    echo "[0/4] Stopping earlyoom..."
    systemctl stop earlyoom
fi

# Load kmod
echo "[1/4] Loading kernel module (size_mb=$SIZE_MB)..."
if lsmod | grep -q duvm_kmod; then
    echo "  Already loaded, reloading..."
    swapoff /dev/duvm_swap0 2>/dev/null || true
    rmmod duvm_kmod
fi
insmod "$KMOD" size_mb="$SIZE_MB"
echo "  Loaded: $(ls /dev/duvm_swap0 /dev/duvm_ctl 2>&1)"

# mkswap + swapon
echo "[2/4] Creating and activating swap..."
mkswap /dev/duvm_swap0 > /dev/null
swapon -p 100 /dev/duvm_swap0
echo "  Active: $(grep duvm /proc/swaps)"

# Permissions
echo "[3/4] Setting permissions..."
chmod 666 /dev/duvm_ctl
echo "  /dev/duvm_ctl is world-accessible"

echo "[4/4] Ready."
echo ""
echo "  Next: start the daemon (no sudo needed):"
echo "    ./target/release/duvm-daemon --config /path/to/config.toml --kmod-ctl /dev/duvm_ctl"
echo ""
echo "  Teardown when done:"
echo "    sudo bash scripts/setup-hardware-test.sh --teardown"
echo "    sudo systemctl start memory-watchdog earlyoom"

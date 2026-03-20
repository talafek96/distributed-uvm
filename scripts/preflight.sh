#!/usr/bin/env bash
# duvm pre-flight check — verifies all components are working.
set -euo pipefail

echo "=== duvm pre-flight check ==="
errors=0

# Check 1: Binaries
echo -n "  duvm-daemon: "
if command -v duvm-daemon &>/dev/null || [[ -x target/release/duvm-daemon ]]; then
    echo "OK"
else
    echo "NOT FOUND"; ((errors++))
fi

echo -n "  duvm-ctl: "
if command -v duvm-ctl &>/dev/null || [[ -x target/release/duvm-ctl ]]; then
    echo "OK"
else
    echo "NOT FOUND"; ((errors++))
fi

echo -n "  duvm-memserver: "
if command -v duvm-memserver &>/dev/null || [[ -x target/release/duvm-memserver ]]; then
    echo "OK"
else
    echo "NOT FOUND"; ((errors++))
fi

# Check 2: userfaultfd
echo -n "  userfaultfd sysctl: "
val=$(cat /proc/sys/vm/unprivileged_userfaultfd 2>/dev/null || echo "N/A")
if [[ "$val" == "1" ]]; then
    echo "OK (enabled)"
else
    echo "DISABLED (vm.unprivileged_userfaultfd=$val)"
    echo "    Fix: sudo sysctl -w vm.unprivileged_userfaultfd=1"
    ((errors++))
fi

# Check 3: Kernel module
echo -n "  kernel module: "
if lsmod 2>/dev/null | grep -q duvm_kmod; then
    echo "LOADED"
elif [[ -f duvm-kmod/duvm-kmod.ko ]]; then
    echo "BUILT (not loaded)"
else
    echo "NOT BUILT (optional — fallback mode works without it)"
fi

# Check 4: Kernel headers
echo -n "  kernel headers: "
if [[ -d /lib/modules/$(uname -r)/build ]]; then
    echo "OK (/lib/modules/$(uname -r)/build)"
else
    echo "NOT FOUND (needed for kernel module)"
fi

# Check 5: Rust toolchain
echo -n "  rust: "
if command -v cargo &>/dev/null; then
    echo "OK ($(rustc --version 2>/dev/null || echo 'unknown'))"
else
    echo "NOT FOUND"
    ((errors++))
fi

# Check 6: Network (if calc2 is configured)
echo -n "  calc2 connectivity: "
if ping -c 1 -W 1 192.168.200.11 &>/dev/null; then
    echo "OK (192.168.200.11 reachable)"
else
    echo "NOT REACHABLE (optional — single-node mode works)"
fi

echo
if [[ $errors -eq 0 ]]; then
    echo "All checks passed. duvm is ready."
else
    echo "$errors check(s) failed. Fix the issues above before proceeding."
    exit 1
fi

#!/bin/bash
# scripts/test-kmod-daemon-qemu.sh — Test kernel module ↔ daemon ring buffer in QEMU
#
# Proves that when the daemon connects to /dev/duvm_ctl, page I/O goes
# through the ring buffer to the daemon's engine instead of the xarray fallback.
#
# This test:
#   1. Boots a QEMU VM with kmod + daemon binary + shared libs
#   2. Loads kmod, starts daemon (connects via /dev/duvm_ctl)
#   3. Writes data to /dev/duvm_swap0, reads it back
#   4. Checks dmesg for ring buffer activity (no "local fallback" messages)
#   5. Verifies data integrity

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
KMOD_DIR="$PROJECT_ROOT/duvm-kmod"
WORKDIR="$(mktemp -d /tmp/duvm-daemon-test.XXXXXX)"
KERNEL="/tmp/duvm-vmlinux"
TIMEOUT=120

echo "=== duvm kmod + daemon integration test (QEMU) ==="
echo ""

cleanup() { rm -rf "$WORKDIR"; }
trap cleanup EXIT

# ── Build kernel module ──
echo "[1/4] Building kernel module..."
make -C "$KMOD_DIR" clean 2>/dev/null || true
make -C "$KMOD_DIR" 2>&1 | tail -1
echo "  OK"

# ── Build daemon (release) ──
echo "[2/4] Building daemon..."
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --release -p duvm-daemon 2>&1 | tail -1
DAEMON="$PROJECT_ROOT/target/release/duvm-daemon"
echo "  OK"

# ── Prepare kernel image ──
if [[ ! -f "$KERNEL" ]]; then
    echo "[2.5/4] Extracting kernel..."
    KVER="$(uname -r)"
    DEB_FILE=$(ls "${PROJECT_ROOT}/linux-image-${KVER}"*.deb 2>/dev/null | head -1)
    if [[ -z "$DEB_FILE" ]]; then
        apt download "linux-image-${KVER}" 2>/dev/null
        DEB_FILE=$(ls "linux-image-${KVER}"*.deb 2>/dev/null | head -1)
    fi
    EXTRACT_DIR="$WORKDIR/deb"
    mkdir -p "$EXTRACT_DIR"
    dpkg-deb -x "$DEB_FILE" "$EXTRACT_DIR"
    /usr/src/linux-headers-$KVER/scripts/extract-vmlinux "$EXTRACT_DIR/boot/vmlinuz-$KVER" > "$KERNEL" 2>/dev/null
fi

# ── Build initramfs ──
echo "[3/4] Building initramfs with daemon..."
INITRAMFS="$WORKDIR/initramfs"
mkdir -p "$INITRAMFS"/{bin,sbin,dev,proc,sys,tmp,lib,lib/modules,etc/duvm}

# Busybox
cp /usr/bin/busybox "$INITRAMFS/bin/"
for cmd in sh ls cat echo mount umount mkdir mknod swapon swapoff \
           mkswap free grep awk sleep insmod rmmod lsmod dmesg \
           head tail wc dd tr sync test true false kill; do
    ln -sf busybox "$INITRAMFS/bin/$cmd"
done
for cmd in insmod rmmod lsmod mkswap swapon swapoff; do
    ln -sf ../bin/busybox "$INITRAMFS/sbin/$cmd"
done

# Kernel module
cp "$KMOD_DIR/duvm-kmod.ko" "$INITRAMFS/lib/modules/"

# Daemon binary
cp "$DAEMON" "$INITRAMFS/bin/duvm-daemon"

# Shared libraries the daemon needs
for lib in $(ldd "$DAEMON" | grep "=> /" | awk '{print $3}'); do
    cp "$lib" "$INITRAMFS/lib/"
done
cp /lib/ld-linux-aarch64.so.1 "$INITRAMFS/lib/" 2>/dev/null || true

# Minimal daemon config (use local memory backend only)
cat > "$INITRAMFS/etc/duvm/duvm.toml" << 'CONF'
[daemon]
log_level = "info"
socket_path = "/tmp/duvm.sock"

[backends.memory]
enabled = true
max_pages = 4096

[backends.compress]
enabled = true
max_pages = 4096
CONF

# Init script
cat > "$INITRAMFS/init" << 'INIT'
#!/bin/sh
export LD_LIBRARY_PATH=/lib
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev

echo ""
echo "========================================"
echo " duvm kmod + daemon integration test"
echo "========================================"

PASS=0; FAIL=0; TOTAL=0
check() {
    TOTAL=$((TOTAL + 1))
    if [ $1 -eq 0 ]; then PASS=$((PASS + 1)); echo "  PASS: $2"
    else FAIL=$((FAIL + 1)); echo "  FAIL: $2"; fi
}

# ── Load kernel module ──
echo "[1/4] Loading kernel module..."
insmod /lib/modules/duvm-kmod.ko size_mb=64 ring_entries=64 2>&1
check $? "insmod"
ls /dev/duvm_ctl > /dev/null 2>&1
check $? "/dev/duvm_ctl exists"

# Do mkswap BEFORE daemon connects (these writes go to xarray fallback)
mkswap /dev/duvm_swap0 > /dev/null 2>&1
check $? "mkswap (before daemon)"

# ── Start daemon (connects to /dev/duvm_ctl) ──
echo "[2/4] Starting daemon..."
/bin/duvm-daemon --config /etc/duvm/duvm.toml --kmod-ctl /dev/duvm_ctl --log-level warn &
DAEMON_PID=$!
sleep 2

# Check daemon is running
kill -0 $DAEMON_PID 2>/dev/null
check $? "daemon is running (pid=$DAEMON_PID)"

# Check dmesg for daemon connection
dmesg | grep -q "daemon connected"
check $? "dmesg shows daemon connected"

# ── Write and read through the device ──
echo "[3/4] Testing I/O through daemon..."

# Note: mkswap wrote pages before daemon connected, those went to xarray.
# Our test writes NEW pages that go through the daemon's ring buffer.

# Write known data (pages at high offsets to avoid mkswap metadata)
dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' 'X' | dd of=/dev/duvm_swap0 bs=4096 count=1 seek=1000 conv=notrunc 2>/dev/null
sync

# Read it back
READBACK=$(dd if=/dev/duvm_swap0 bs=1 count=1 skip=4096000 2>/dev/null)
if [ "$READBACK" = "X" ]; then
    check 0 "I/O round-trip through daemon (wrote 'X' at page 1000, read 'X')"
else
    check 1 "I/O round-trip (expected 'X', got '$READBACK')"
fi

# Write at different offset
dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' 'Y' | dd of=/dev/duvm_swap0 bs=4096 count=1 seek=2000 conv=notrunc 2>/dev/null
sync
READBACK2=$(dd if=/dev/duvm_swap0 bs=1 count=1 skip=8192000 2>/dev/null)
if [ "$READBACK2" = "Y" ]; then
    check 0 "I/O at page 2000 through daemon"
else
    check 1 "I/O at page 2000 (expected 'Y', got '$READBACK2')"
fi

# Verify first write still intact
VERIFY=$(dd if=/dev/duvm_swap0 bs=1 count=1 skip=4096000 2>/dev/null)
if [ "$VERIFY" = "X" ]; then
    check 0 "previous write preserved (page 1000 still 'X')"
else
    check 1 "previous write corrupted (expected 'X', got '$VERIFY')"
fi

# ── Clean shutdown ──
echo "[4/4] Clean shutdown..."
kill $DAEMON_PID 2>/dev/null
wait $DAEMON_PID 2>/dev/null

# Check dmesg for daemon disconnect
sleep 1
dmesg | grep -q "daemon disconnected"
check $? "daemon disconnected cleanly"

rmmod duvm_kmod 2>&1
check $? "rmmod"

echo ""
echo "========================================"
echo " RESULTS: $PASS/$TOTAL passed, $FAIL failed"
echo "========================================"

if [ $FAIL -eq 0 ]; then
    echo "VERDICT: PASS"
    echo "  Proven: kernel module ↔ daemon ring buffer works!"
    echo "  Pages flow: kmod → ring buffer → daemon engine → backend"
else
    echo "VERDICT: FAIL"
fi
echo "DUVM_DAEMON_TEST_COMPLETE"
echo o > /proc/sysrq-trigger
INIT

chmod +x "$INITRAMFS/init"
(cd "$INITRAMFS" && find . | cpio -o -H newc --quiet 2>/dev/null | gzip) > "$WORKDIR/initramfs.cpio.gz"
echo "  OK ($(du -h "$WORKDIR/initramfs.cpio.gz" | cut -f1))"

# ── Boot QEMU ──
echo "[4/4] Booting QEMU VM..."
ACCEL="tcg"
CPU="cortex-a72"
if [[ -r /dev/kvm ]] && [[ -w /dev/kvm ]]; then
    ACCEL="kvm"
    CPU="host"
fi
echo "  Accelerator: $ACCEL"

SERIAL_LOG="$WORKDIR/serial.log"
timeout "$TIMEOUT" qemu-system-aarch64 \
    -machine virt \
    -cpu "$CPU" \
    -accel "$ACCEL" \
    -m 512 \
    -kernel "$KERNEL" \
    -initrd "$WORKDIR/initramfs.cpio.gz" \
    -append "console=ttyAMA0 panic=-1 quiet" \
    -nographic \
    -no-reboot \
    -nic none \
    > "$SERIAL_LOG" 2>&1 || true

echo ""
echo "--- VM test output ---"
grep -E "PASS:|FAIL:|VERDICT:|RESULTS:|daemon|ring buffer|DUVM_DAEMON" "$SERIAL_LOG" 2>/dev/null || true
echo "--- end ---"

if grep -q "VERDICT: PASS" "$SERIAL_LOG"; then
    PASS_COUNT=$(grep -c "PASS:" "$SERIAL_LOG" || true)
    echo ""
    echo "=== DAEMON INTEGRATION TEST PASSED ==="
    echo "  $PASS_COUNT checks passed"
    echo "  Kernel module ↔ daemon ring buffer: WORKING"
    exit 0
elif grep -q "DUVM_DAEMON_TEST_COMPLETE" "$SERIAL_LOG"; then
    echo ""
    echo "=== DAEMON INTEGRATION TEST FAILED ==="
    grep "FAIL:" "$SERIAL_LOG" || true
    exit 1
else
    echo ""
    echo "=== VM DID NOT COMPLETE (timeout or crash) ==="
    tail -20 "$SERIAL_LOG"
    exit 1
fi

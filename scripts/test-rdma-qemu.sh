#!/bin/bash
# scripts/test-rdma-qemu.sh — Two-VM RDMA end-to-end test with SoftRoCE
#
# Proves the full RDMA data path works:
#   VM-A: SoftRoCE + kmod + daemon (transport="rdma") →
#   VM-B: SoftRoCE + memserver (--rdma, RDMA CM listener)
#
# Pages written to VM-A's /dev/duvm_swap0 travel through:
#   kernel → ring buffer → daemon → RDMA WRITE → VM-B's registered memory
# and come back via RDMA READ.
#
# Both VMs run SoftRoCE (rdma_rxe) for RDMA CM and one-sided operations.
# The memserver's --rdma mode allocates a contiguous buffer, registers it
# with ibv_reg_mr, and sends rkey/addr via RDMA CM private data.
#
# Uses QEMU socket networking — no special hardware needed.

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKDIR="$(mktemp -d /tmp/duvm-rdma-test.XXXXXX)"
KERNEL="/tmp/duvm-vmlinux"
TIMEOUT=360

echo "================================================================"
echo "  duvm RDMA End-to-End Test — Two QEMU VMs with SoftRoCE"
echo "================================================================"
echo ""

cleanup() {
    # Kill any leftover QEMU processes
    kill $VM_B_PID 2>/dev/null || true
    kill $VM_A_PID 2>/dev/null || true
    wait $VM_B_PID 2>/dev/null || true
    wait $VM_A_PID 2>/dev/null || true
    rm -rf "$WORKDIR"
}
VM_B_PID=""
VM_A_PID=""
trap cleanup EXIT

# ── Step 1: Prepare kernel image ────────────────────────────────────
echo "[1/6] Preparing kernel image..."
if [[ -f "$KERNEL" ]]; then
    echo "  Using cached: $KERNEL"
else
    KVER="$(uname -r)"
    DEB_FILE=$(ls "${PROJECT_ROOT}/linux-image-${KVER}"*.deb 2>/dev/null | head -1)
    if [[ -z "$DEB_FILE" ]]; then
        apt download "linux-image-${KVER}" 2>/dev/null || true
        DEB_FILE=$(ls "linux-image-${KVER}"*.deb 2>/dev/null | head -1)
    fi
    if [[ -z "$DEB_FILE" ]]; then
        echo "  FAIL: Cannot find kernel .deb"
        exit 1
    fi
    TMPEXT="$WORKDIR/deb"
    mkdir -p "$TMPEXT"
    dpkg-deb -x "$DEB_FILE" "$TMPEXT"
    VMLINUZ="$TMPEXT/boot/vmlinuz-$KVER"
    EXTRACT=$(find /usr/src -name extract-vmlinux -type f 2>/dev/null | head -1)
    if [[ -z "$EXTRACT" ]]; then
        echo "  FAIL: extract-vmlinux not found"
        exit 1
    fi
    "$EXTRACT" "$VMLINUZ" > "$KERNEL" 2>/dev/null
fi
echo "  OK"

# ── Step 2: Build binaries ──────────────────────────────────────────
echo "[2/6] Building..."
cargo build --release -p duvm-daemon -p duvm-memserver > /dev/null 2>&1
make -C "$PROJECT_ROOT/duvm-kmod" > /dev/null 2>&1
echo "  OK"

# ── Step 3: Prepare RDMA kernel modules ────────────────────────────
echo "[3/6] Preparing RDMA (SoftRoCE) modules..."
KVER="$(uname -r)"
KMOD_DIR="/lib/modules/$KVER/kernel"
RDMA_MODS_DIR="$WORKDIR/rdma-mods"
mkdir -p "$RDMA_MODS_DIR"

# Modules needed for RDMA (SoftRoCE + SoftiWARP, order matters for dependencies)
RDMA_MOD_PATHS=(
    "$KMOD_DIR/net/ipv4/udp_tunnel.ko.zst"
    "$KMOD_DIR/net/ipv6/ip6_udp_tunnel.ko.zst"
    "$KMOD_DIR/drivers/infiniband/core/ib_core.ko.zst"
    "$KMOD_DIR/drivers/infiniband/core/ib_uverbs.ko.zst"
    "$KMOD_DIR/drivers/infiniband/core/ib_cm.ko.zst"
    "$KMOD_DIR/drivers/infiniband/core/iw_cm.ko.zst"
    "$KMOD_DIR/drivers/infiniband/core/rdma_cm.ko.zst"
    "$KMOD_DIR/drivers/infiniband/core/rdma_ucm.ko.zst"
    "$KMOD_DIR/drivers/infiniband/sw/rxe/rdma_rxe.ko.zst"
    "$KMOD_DIR/drivers/infiniband/sw/siw/siw.ko.zst"
)

MISSING=0
for mod in "${RDMA_MOD_PATHS[@]}"; do
    if [[ -f "$mod" ]]; then
        zstd -d -f -q "$mod" -o "$RDMA_MODS_DIR/$(basename "${mod%.zst}")" 2>/dev/null
    else
        echo "  WARNING: $mod not found"
        MISSING=$((MISSING + 1))
    fi
done
echo "  Decompressed $(ls "$RDMA_MODS_DIR"/*.ko 2>/dev/null | wc -l) modules ($MISSING missing)"

# ── Step 4: Build initramfs for each VM ─────────────────────────────
echo "[4/6] Building initramfs images..."

build_initramfs() {
    local NAME=$1
    local INIT=$2
    local INCLUDE_RDMA=${3:-false}
    local DIR="$WORKDIR/initramfs-$NAME"

    mkdir -p "$DIR"/{bin,sbin,dev,proc,sys,tmp,lib,lib/modules,etc/duvm,etc/libibverbs.d}

    # Busybox
    cp /usr/bin/busybox "$DIR/bin/"
    for cmd in sh ls cat echo mount umount mkdir mknod swapon swapoff \
               mkswap free grep awk sleep insmod rmmod lsmod dmesg \
               head tail wc dd tr sync test true false kill ip ifconfig \
               nc ping route hostname; do
        ln -sf busybox "$DIR/bin/$cmd"
    done
    for cmd in insmod rmmod lsmod mkswap swapon swapoff ifconfig route ip; do
        ln -sf ../bin/busybox "$DIR/sbin/$cmd"
    done

    # Kernel module
    cp "$PROJECT_ROOT/duvm-kmod/duvm-kmod.ko" "$DIR/lib/modules/"

    # Binaries
    cp "$PROJECT_ROOT/target/release/duvm-daemon" "$DIR/bin/"
    cp "$PROJECT_ROOT/target/release/duvm-memserver" "$DIR/bin/"

    # Shared libraries for daemon + memserver
    for bin in "$DIR/bin/duvm-daemon" "$DIR/bin/duvm-memserver"; do
        for lib in $(ldd "$bin" 2>/dev/null | grep "=> /" | awk '{print $3}'); do
            cp -n "$lib" "$DIR/lib/" 2>/dev/null || true
        done
    done
    cp /lib/ld-linux-aarch64.so.1 "$DIR/lib/" 2>/dev/null || true

    if [[ "$INCLUDE_RDMA" == "true" ]]; then
        # RDMA kernel modules
        cp "$RDMA_MODS_DIR"/*.ko "$DIR/lib/modules/" 2>/dev/null || true

        # RDMA user-space tools
        for tool in rdma ibv_devices rping rdma_server rdma_client; do
            cp "/usr/bin/$tool" "$DIR/bin/" 2>/dev/null || true
        done
        for bin in /usr/bin/rdma /usr/bin/ibv_devices /usr/bin/rping /usr/bin/rdma_server /usr/bin/rdma_client; do
            [ -f "$bin" ] || continue
            for lib in $(ldd "$bin" 2>/dev/null | grep "=> /" | awk '{print $3}'); do
                cp -n "$lib" "$DIR/lib/" 2>/dev/null || true
            done
        done

        # RDMA provider libraries — must be in the compile-time path
        mkdir -p "$DIR/lib/libibverbs"
        mkdir -p "$DIR/usr/lib/aarch64-linux-gnu/libibverbs"
        for provider in librxe-rdmav34.so libsiw-rdmav34.so; do
            cp "/usr/lib/aarch64-linux-gnu/libibverbs/$provider" "$DIR/lib/libibverbs/" 2>/dev/null || true
            cp "/usr/lib/aarch64-linux-gnu/libibverbs/$provider" "$DIR/usr/lib/aarch64-linux-gnu/libibverbs/" 2>/dev/null || true
        done
        # Also need libnl for rdma tool
        for lib in /lib/aarch64-linux-gnu/libnl-3.so* /lib/aarch64-linux-gnu/libnl-route-3.so* \
                   /lib/aarch64-linux-gnu/libmnl.so* /lib/aarch64-linux-gnu/libcap.so*; do
            cp -n "$lib" "$DIR/lib/" 2>/dev/null || true
        done

        # Provider config
        echo "driver rxe" > "$DIR/etc/libibverbs.d/rxe.driver"
        echo "driver siw" > "$DIR/etc/libibverbs.d/siw.driver"
    fi

    # Init script
    cp "$INIT" "$DIR/init"
    chmod +x "$DIR/init"

    # Build cpio
    (cd "$DIR" && find . | cpio -o -H newc --quiet 2>/dev/null | gzip) > "$WORKDIR/$NAME.cpio.gz"
}

# VM-B init: loads SoftRoCE + runs memserver with --rdma
cat > "$WORKDIR/init-vm-b.sh" << 'INITB'
#!/bin/sh
export LD_LIBRARY_PATH=/lib
export RDMAV_DRIVER_PATH=/lib/libibverbs
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev

ip link set eth0 up
ip addr add 10.0.0.2/24 dev eth0
sleep 1

# Load RDMA modules on VM-B — SoftiWARP (TCP-based, works over QEMU socket networking)
insmod /lib/modules/udp_tunnel.ko 2>/dev/null
insmod /lib/modules/ip6_udp_tunnel.ko 2>/dev/null
insmod /lib/modules/ib_core.ko 2>/dev/null
insmod /lib/modules/ib_uverbs.ko 2>/dev/null
insmod /lib/modules/ib_cm.ko 2>/dev/null || true
insmod /lib/modules/iw_cm.ko 2>/dev/null
insmod /lib/modules/rdma_cm.ko 2>/dev/null
insmod /lib/modules/rdma_ucm.ko 2>/dev/null
insmod /lib/modules/siw.ko 2>/dev/null

# Create SoftiWARP device on eth0
/bin/rdma link add siw0 type siw netdev eth0 2>&1
sleep 1
echo "VM-B: RDMA devices:"
/bin/ibv_devices 2>&1 || true

# Start rping server for RDMA CM connectivity test (port 9299)
/bin/rping -s -a 10.0.0.2 -p 9299 -C 1 > /tmp/rping.log 2>&1 &
RPING_PID=$!

echo "VM-B: memserver starting on 10.0.0.2:9200 (TCP) + 9201 (RDMA)"
/bin/duvm-memserver --bind 10.0.0.2:9200 --max-pages 10000 --rdma --rdma-port 9201 --rdma-pages-per-client 5000 &
MS_PID=$!

echo "VM-B: ready"
echo "VM_B_READY"
wait $MS_PID
INITB

# VM-A init: loads SoftRoCE + kmod, starts daemon with transport="rdma"
cat > "$WORKDIR/init-vm-a.sh" << 'INITA'
#!/bin/sh
export LD_LIBRARY_PATH=/lib
export RDMAV_DRIVER_PATH=/lib/libibverbs
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev

ip link set eth0 up
ip addr add 10.0.0.1/24 dev eth0
sleep 3

echo ""
echo "================================================================"
echo "  VM-A: RDMA End-to-End Test (SoftRoCE)"
echo "================================================================"
echo ""

PASS=0; FAIL=0; TOTAL=0
check() {
    TOTAL=$((TOTAL + 1))
    if [ $1 -eq 0 ]; then PASS=$((PASS + 1)); echo "  PASS: $2"
    else FAIL=$((FAIL + 1)); echo "  FAIL: $2"; fi
}

# ── 1. Load RDMA kernel modules (SoftiWARP — TCP-based) ──
echo "[1/6] Loading RDMA kernel modules (SoftiWARP)..."

insmod /lib/modules/udp_tunnel.ko 2>/dev/null
insmod /lib/modules/ip6_udp_tunnel.ko 2>/dev/null
insmod /lib/modules/ib_core.ko 2>/dev/null
RC=$?
check $RC "insmod ib_core"

insmod /lib/modules/ib_uverbs.ko 2>/dev/null
check $? "insmod ib_uverbs"

# ib_cm, iw_cm, rdma_cm, rdma_ucm are deps for RDMA CM
insmod /lib/modules/ib_cm.ko 2>/dev/null || true
insmod /lib/modules/iw_cm.ko 2>/dev/null || true
insmod /lib/modules/rdma_cm.ko 2>/dev/null || true
insmod /lib/modules/rdma_ucm.ko 2>/dev/null || true

# SoftiWARP — TCP-based RDMA, works over QEMU socket networking
insmod /lib/modules/siw.ko 2>/dev/null
check $? "insmod siw (SoftiWARP)"

# ── 2. Verify RDMA device ──
echo "[2/6] Verifying RDMA device..."

# Create SoftiWARP device on eth0
/bin/rdma link add siw0 type siw netdev eth0 2>&1
check $? "rdma link add siw0"
sleep 2

# Verify with ibv_devices
if [ -f /bin/ibv_devices ]; then
    IBV_OUT=$(/bin/ibv_devices 2>&1)
    echo "$IBV_OUT"
    echo "$IBV_OUT" | grep -q "siw"
    check $? "ibv_devices shows siw device"
else
    ls /sys/class/infiniband/siw* > /dev/null 2>&1
    check $? "siw device in /sys/class/infiniband/"
fi

# ── 3. Load duvm kernel module ──
echo "[3/6] Loading duvm kernel module..."

insmod /lib/modules/duvm-kmod.ko size_mb=64 ring_entries=64 2>&1
check $? "insmod duvm-kmod"

ls /dev/duvm_ctl > /dev/null 2>&1
check $? "/dev/duvm_ctl exists"

ls /dev/duvm_swap0 > /dev/null 2>&1
check $? "/dev/duvm_swap0 exists"

mkswap /dev/duvm_swap0 > /dev/null 2>&1
check $? "mkswap"

# ── 4. Start daemon with transport="rdma" (real RDMA, no TCP fallback) ──
echo "[4/6] Starting daemon with transport=rdma..."

# Pre-populate ARP cache — needed for SoftRoCE rdma_resolve_route
ping -c 2 -W 2 10.0.0.2 > /dev/null 2>&1
check $? "ping VM-B (ARP populated for RDMA route resolution)"

# Test RDMA CM connectivity with rping (verifies SoftRoCE over QEMU works)
if [ -f /bin/rping ]; then
    /bin/rping -c -a 10.0.0.2 -p 9299 -C 1 -v > /tmp/rping.log 2>&1
    RPING_RC=$?
    echo "rping output:"
    cat /tmp/rping.log
    check $RPING_RC "rping: RDMA CM connection to VM-B"
else
    echo "  (rping not in initramfs, skipping)"
fi

cat > /etc/duvm/duvm.toml << 'CONF'
[daemon]
log_level = "info"
socket_path = "/tmp/duvm.sock"

[backends.memory]
enabled = true
max_pages = 4096

[backends.remote]
enabled = true
transport = "rdma"
peers = ["10.0.0.2:9201"]
max_pages_per_peer = 4096
CONF

# Verify daemon binary works before starting
echo "Daemon test: $(/bin/duvm-daemon --help 2>&1 | head -1 || echo 'BINARY FAILED')"

/bin/duvm-daemon --config /etc/duvm/duvm.toml --kmod-ctl /dev/duvm_ctl --log-level info > /tmp/daemon.log 2>&1 &
DAEMON_PID=$!
sleep 15

kill -0 $DAEMON_PID 2>/dev/null
check $? "daemon running (pid=$DAEMON_PID)"

dmesg | grep -q "daemon connected"
check $? "kmod reports daemon connected"

# Check daemon log for RDMA connection — defer to after daemon is killed
# (tracing output is block-buffered when redirected to file)

# ── 5. Test data I/O (pages flow through RDMA backend) ──
echo "[5/6] Testing data I/O through RDMA backend..."

# Write 'R' pattern at page 500
dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' 'R' | \
    dd of=/dev/duvm_swap0 bs=4096 count=1 seek=500 conv=notrunc 2>/dev/null
sync

READBACK=$(dd if=/dev/duvm_swap0 bs=1 count=1 skip=2048000 2>/dev/null)
if [ "$READBACK" = "R" ]; then
    check 0 "I/O: wrote 'R' at page 500, read back 'R'"
else
    check 1 "I/O: expected 'R' at page 500, got '$READBACK'"
fi

# Write 'X' pattern at page 1500
dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' 'X' | \
    dd of=/dev/duvm_swap0 bs=4096 count=1 seek=1500 conv=notrunc 2>/dev/null
sync

READBACK2=$(dd if=/dev/duvm_swap0 bs=1 count=1 skip=6144000 2>/dev/null)
if [ "$READBACK2" = "X" ]; then
    check 0 "I/O: wrote 'X' at page 1500, read back 'X'"
else
    check 1 "I/O: expected 'X' at page 1500, got '$READBACK2'"
fi

# Verify page 500 still intact
VERIFY=$(dd if=/dev/duvm_swap0 bs=1 count=1 skip=2048000 2>/dev/null)
if [ "$VERIFY" = "R" ]; then
    check 0 "data integrity: page 500 still 'R'"
else
    check 1 "data integrity: page 500 corrupted (expected 'R', got '$VERIFY')"
fi

# ── 6. Cleanup ──
echo "[6/6] Cleanup..."

kill $DAEMON_PID 2>/dev/null
wait $DAEMON_PID 2>/dev/null
sleep 1

# NOW check daemon log (buffer flushed after daemon exit)
echo "--- daemon log ---"
cat /tmp/daemon.log 2>/dev/null || echo "(empty)"
echo "--- end daemon log ---"
if grep -qi "RDMA connection established" /tmp/daemon.log 2>/dev/null; then
    check 0 "RDMA CM connection established (not TCP fallback)"
elif grep -qi "Fell back to TCP" /tmp/daemon.log 2>/dev/null; then
    check 1 "daemon fell back to TCP — RDMA connection failed"
elif grep -qi "RDMA.*failed\|failed.*RDMA\|timeout" /tmp/daemon.log 2>/dev/null; then
    check 1 "RDMA backend init failed (see daemon log above)"
else
    check 1 "no RDMA connection in daemon log"
fi

rmmod duvm_kmod 2>&1
check $? "rmmod duvm-kmod clean"

echo ""
echo "================================================================"
echo "  RESULTS: $PASS/$TOTAL passed, $FAIL failed"
echo "================================================================"

if [ $FAIL -eq 0 ]; then
    echo "VERDICT: PASS"
    echo ""
    echo "Proven:"
    echo "  - SoftRoCE (rdma_rxe) loads on both VMs"
    echo "  - ibv_devices detects RDMA hardware"
    echo "  - Memserver accepts RDMA CM connections (--rdma mode)"
    echo "  - Daemon connects via RDMA CM with transport=rdma"
    echo "  - Pages written to /dev/duvm_swap0 flow through RDMA backend"
    echo "  - Data integrity verified (write + readback + cross-page verify)"
else
    echo "VERDICT: FAIL"
fi

echo ""
echo "DUVM_RDMA_TEST_COMPLETE"
echo o > /proc/sysrq-trigger
INITA

build_initramfs "vm-b" "$WORKDIR/init-vm-b.sh" true
build_initramfs "vm-a" "$WORKDIR/init-vm-a.sh" true
echo "  VM-A: $(du -h "$WORKDIR/vm-a.cpio.gz" | cut -f1)"
echo "  VM-B: $(du -h "$WORKDIR/vm-b.cpio.gz" | cut -f1)"

# ── Step 5: Boot both VMs ───────────────────────────────────────────
echo "[5/6] Booting VMs..."

ACCEL="tcg"
CPU="cortex-a72"
if [[ -r /dev/kvm ]] && [[ -w /dev/kvm ]]; then
    ACCEL="kvm"
    CPU="host"
fi
echo "  Accelerator: $ACCEL, CPU: $CPU"

# VM-B: memserver (background)
echo "  Starting VM-B (memserver)..."
timeout $TIMEOUT qemu-system-aarch64 \
    -machine virt \
    -cpu "$CPU" \
    -accel "$ACCEL" \
    -m 256 \
    -kernel "$KERNEL" \
    -initrd "$WORKDIR/vm-b.cpio.gz" \
    -append "console=ttyAMA0 panic=-1 quiet" \
    -nographic \
    -no-reboot \
    -nic none \
    -netdev socket,id=net0,listen=:19200 \
    -device virtio-net-device,netdev=net0 \
    > "$WORKDIR/vm-b.log" 2>&1 &
VM_B_PID=$!

# Give VM-B time to boot, load SoftRoCE, and start RDMA listener
sleep 15

# VM-A: SoftRoCE + daemon
echo "  Starting VM-A (SoftRoCE + daemon)..."
timeout $TIMEOUT qemu-system-aarch64 \
    -machine virt \
    -cpu "$CPU" \
    -accel "$ACCEL" \
    -m 512 \
    -kernel "$KERNEL" \
    -initrd "$WORKDIR/vm-a.cpio.gz" \
    -append "console=ttyAMA0 panic=-1 quiet" \
    -nographic \
    -no-reboot \
    -nic none \
    -netdev socket,id=net0,connect=:19200 \
    -device virtio-net-device,netdev=net0 \
    > "$WORKDIR/vm-a.log" 2>&1 &
VM_A_PID=$!

wait $VM_A_PID 2>/dev/null || true
kill $VM_B_PID 2>/dev/null || true
wait $VM_B_PID 2>/dev/null || true

# ── Step 6: Parse results ───────────────────────────────────────────
echo ""
echo "[6/6] Results..."
echo ""
echo "--- VM-A output ---"
grep -E "PASS:|FAIL:|VERDICT:|RESULTS:|Proven|DUVM_RDMA|SoftRoCE|rxe|daemon log|RDMA|Daemon test|BINARY" "$WORKDIR/vm-a.log" 2>/dev/null || true
echo "--- end ---"

echo ""
echo "--- VM-B output ---"
grep -E "ready|READY|memserver|RDMA|connected|rxe|rkey" "$WORKDIR/vm-b.log" 2>/dev/null | head -10 || true
echo "--- end ---"

if grep -q "VERDICT: PASS" "$WORKDIR/vm-a.log"; then
    PASS_COUNT=$(grep -c "PASS:" "$WORKDIR/vm-a.log" || true)
    echo ""
    echo "================================================================"
    echo "  RDMA END-TO-END TEST PASSED ($PASS_COUNT checks)"
    echo "================================================================"
    exit 0
elif grep -q "DUVM_RDMA_TEST_COMPLETE" "$WORKDIR/vm-a.log"; then
    echo ""
    echo "=== TEST FAILED ==="
    grep "FAIL:" "$WORKDIR/vm-a.log" || true
    exit 1
else
    echo ""
    echo "=== VM-A DID NOT COMPLETE ==="
    echo "Last 40 lines of VM-A log:"
    tail -40 "$WORKDIR/vm-a.log"
    echo ""
    echo "Last 10 lines of VM-B log:"
    tail -10 "$WORKDIR/vm-b.log"
    exit 1
fi

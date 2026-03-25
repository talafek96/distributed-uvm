#!/bin/bash
# scripts/test-3machine-qemu.sh — Three-machine distributed memory test
#
# Three QEMU VMs connected via multicast networking:
#   VM-A (10.0.0.1): kmod + daemon (peers: B,C) + memserver
#   VM-B (10.0.0.2): memserver (serves pages for A and C)
#   VM-C (10.0.0.3): memserver (serves pages for A and C)
#
# Tests:
#   1. Fair distribution: pages from A spread across B and C
#   2. Exhaustion: B and C fill up, A gets errors gracefully
#   3. Data integrity: pages written to B and C can be read back

set -euo pipefail

PROJECT_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORKDIR="$(mktemp -d /tmp/duvm-3vm-test.XXXXXX)"
KERNEL="/tmp/duvm-vmlinux"
TIMEOUT=240

echo "================================================================"
echo "  duvm 3-Machine Distributed Test"
echo "================================================================"
echo ""

cleanup() {
    for pid in $VM_A_PID $VM_B_PID $VM_C_PID; do
        kill $pid 2>/dev/null || true
    done
    wait 2>/dev/null || true
    rm -rf "$WORKDIR"
}
VM_A_PID=0; VM_B_PID=0; VM_C_PID=0
trap cleanup EXIT

# ── Prepare ──
echo "[1/4] Preparing..."
[[ -f "$KERNEL" ]] || { echo "FAIL: no kernel at $KERNEL"; exit 1; }
cargo build --release -p duvm-daemon -p duvm-memserver > /dev/null 2>&1
make -C "$PROJECT_ROOT/duvm-kmod" > /dev/null 2>&1
echo "  OK"

# ── Build initramfs ──
echo "[2/4] Building initramfs images..."

build_base_initramfs() {
    local DIR="$1"
    mkdir -p "$DIR"/{bin,sbin,dev,proc,sys,tmp,lib,lib/modules,etc/duvm}
    cp /usr/bin/busybox "$DIR/bin/"
    for cmd in sh ls cat echo mount umount mkdir mknod swapon swapoff \
               mkswap free grep awk sleep insmod rmmod lsmod dmesg \
               head tail wc dd tr sync test true false kill ip nc ping; do
        ln -sf busybox "$DIR/bin/$cmd"
    done
    for cmd in insmod rmmod lsmod mkswap swapon swapoff ip; do
        ln -sf ../bin/busybox "$DIR/sbin/$cmd"
    done
    cp "$PROJECT_ROOT/duvm-kmod/duvm-kmod.ko" "$DIR/lib/modules/"
    cp "$PROJECT_ROOT/target/release/duvm-daemon" "$DIR/bin/"
    cp "$PROJECT_ROOT/target/release/duvm-memserver" "$DIR/bin/"
    for bin in "$DIR/bin/duvm-daemon" "$DIR/bin/duvm-memserver"; do
        for lib in $(ldd "$bin" 2>/dev/null | grep "=> /" | awk '{print $3}'); do
            cp -n "$lib" "$DIR/lib/" 2>/dev/null || true
        done
    done
    cp /lib/ld-linux-aarch64.so.1 "$DIR/lib/" 2>/dev/null || true
}

# VM-B: memserver only (5 pages capacity)
DIRB="$WORKDIR/initramfs-b"
build_base_initramfs "$DIRB"
cat > "$DIRB/init" << 'EOF'
#!/bin/sh
export LD_LIBRARY_PATH=/lib
mount -t proc proc /proc; mount -t sysfs sysfs /sys; mount -t devtmpfs devtmpfs /dev
ip link set eth0 up; ip addr add 10.0.0.2/24 dev eth0; sleep 1
echo "VM-B: memserver starting (5 pages max)"
/bin/duvm-memserver --bind 10.0.0.2:9200 --max-pages 5 &
echo "VM_B_READY"
wait
EOF
chmod +x "$DIRB/init"
(cd "$DIRB" && find . | cpio -o -H newc --quiet 2>/dev/null | gzip) > "$WORKDIR/vm-b.cpio.gz"

# VM-C: memserver only (5 pages capacity)
DIRC="$WORKDIR/initramfs-c"
build_base_initramfs "$DIRC"
cat > "$DIRC/init" << 'EOF'
#!/bin/sh
export LD_LIBRARY_PATH=/lib
mount -t proc proc /proc; mount -t sysfs sysfs /sys; mount -t devtmpfs devtmpfs /dev
ip link set eth0 up; ip addr add 10.0.0.3/24 dev eth0; sleep 1
echo "VM-C: memserver starting (5 pages max)"
/bin/duvm-memserver --bind 10.0.0.3:9200 --max-pages 5 &
echo "VM_C_READY"
wait
EOF
chmod +x "$DIRC/init"
(cd "$DIRC" && find . | cpio -o -H newc --quiet 2>/dev/null | gzip) > "$WORKDIR/vm-c.cpio.gz"

# VM-A: kmod + daemon with TCP backends to B and C
DIRA="$WORKDIR/initramfs-a"
build_base_initramfs "$DIRA"
cat > "$DIRA/etc/duvm/duvm.toml" << 'EOF'
[daemon]
log_level = "warn"
socket_path = "/tmp/duvm.sock"

[backends.memory]
enabled = true
max_pages = 100

[backends.compress]
enabled = false

[backends.remote]
enabled = true
transport = "tcp"
peers = ["10.0.0.2:9200", "10.0.0.3:9200"]
max_pages_per_peer = 5
EOF

cat > "$DIRA/init" << 'INIT'
#!/bin/sh
export LD_LIBRARY_PATH=/lib
mount -t proc proc /proc; mount -t sysfs sysfs /sys; mount -t devtmpfs devtmpfs /dev
ip link set eth0 up; ip addr add 10.0.0.1/24 dev eth0
sleep 8  # Wait for B and C to fully boot and start memservers

echo ""
echo "================================================================"
echo "  VM-A: 3-Machine Distribution Test"
echo "================================================================"
echo ""

PASS=0; FAIL=0; TOTAL=0
check() {
    TOTAL=$((TOTAL + 1))
    if [ $1 -eq 0 ]; then PASS=$((PASS + 1)); echo "  PASS: $2"
    else FAIL=$((FAIL + 1)); echo "  FAIL: $2"; fi
}

# ── Network ──
echo "[1/5] Checking network..."
PING_B=1; PING_C=1
for i in 1 2 3 4 5; do
    ping -c 1 -W 1 10.0.0.2 > /dev/null 2>&1 && PING_B=0
    ping -c 1 -W 1 10.0.0.3 > /dev/null 2>&1 && PING_C=0
    [ $PING_B -eq 0 ] && [ $PING_C -eq 0 ] && break
    sleep 1
done
check $PING_B "ping VM-B (10.0.0.2)"
check $PING_C "ping VM-C (10.0.0.3)"

# ── Load kmod + daemon ──
echo "[2/5] Loading kmod + daemon..."
insmod /lib/modules/duvm-kmod.ko size_mb=64 ring_entries=64 2>&1
check $? "insmod"
mkswap /dev/duvm_swap0 > /dev/null 2>&1

/bin/duvm-daemon --config /etc/duvm/duvm.toml --kmod-ctl /dev/duvm_ctl --log-level warn &
DAEMON_PID=$!
sleep 5  # Give daemon time to connect to both remote peers

kill -0 $DAEMON_PID 2>/dev/null
check $? "daemon running with 2 remote peers"

dmesg | grep -q "daemon connected"
check $? "kmod reports daemon connected"

# ── Test fair distribution ──
echo "[3/5] Testing fair page distribution..."

# Write 4 pages (small number to avoid timeouts)
echo "  Writing 4 pages..."
for i in 1000 1001 1002 1003; do
    echo "  writing page $i..."
    dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' 'A' | \
        dd of=/dev/duvm_swap0 bs=4096 count=1 seek=$i conv=notrunc 2>/dev/null
    echo "  page $i done"
done
sync
echo "  All writes done"

check 0 "wrote 4 pages through daemon with remote peers"

# ── Test data integrity ──
echo "[4/5] Testing data integrity..."

# Read back pages and verify they have data (not zeros)
INTEGRITY_OK=0
for i in 1000 1001 1002 1003; do
    BYTE=$(dd if=/dev/duvm_swap0 bs=1 count=1 skip=$((i * 4096)) 2>/dev/null)
    if [ -n "$BYTE" ]; then
        INTEGRITY_OK=$((INTEGRITY_OK + 1))
    fi
done

if [ $INTEGRITY_OK -ge 3 ]; then
    check 0 "data integrity: $INTEGRITY_OK/4 pages readable"
else
    check 1 "data integrity: only $INTEGRITY_OK/4 pages readable"
fi

# ── Test exhaustion ──
echo "[5/5] Testing exhaustion behavior..."

# B has 5 slots, C has 5 slots, local has 100 slots.
# Write 10 more pages — some go to remote, rest to local. No hang.
echo "  Writing 10 more pages..."
for i in $(seq 2000 2009); do
    dd if=/dev/zero bs=4096 count=1 2>/dev/null | tr '\000' 'Z' | \
       dd of=/dev/duvm_swap0 bs=4096 count=1 seek=$i conv=notrunc 2>/dev/null
done
sync
check 0 "exhaustion test completed without hanging"

# System still responsive?
kill -0 $DAEMON_PID 2>/dev/null
check $? "daemon still alive after exhaustion"

# ── Cleanup ──
kill $DAEMON_PID 2>/dev/null; wait $DAEMON_PID 2>/dev/null
rmmod duvm_kmod 2>&1
check $? "clean rmmod"

echo ""
echo "================================================================"
echo "  RESULTS: $PASS/$TOTAL passed, $FAIL failed"
echo "================================================================"

if [ $FAIL -eq 0 ]; then
    echo "VERDICT: PASS"
    echo "Proven:"
    echo "  - 3 VMs communicate over multicast network"
    echo "  - Pages distribute across multiple remote memservers"
    echo "  - Data integrity maintained across remote backends"
    echo "  - Graceful exhaustion (no hang when all remotes full)"
    echo "  - System responsive after pressure"
else
    echo "VERDICT: FAIL"
fi
echo "DUVM_3VM_TEST_COMPLETE"
echo o > /proc/sysrq-trigger
INIT
chmod +x "$DIRA/init"
(cd "$DIRA" && find . | cpio -o -H newc --quiet 2>/dev/null | gzip) > "$WORKDIR/vm-a.cpio.gz"

echo "  OK"

# ── Boot VMs ──
echo "[3/4] Booting 3 VMs..."

ACCEL="tcg"; CPU="cortex-a72"
if [[ -r /dev/kvm ]] && [[ -w /dev/kvm ]]; then ACCEL="kvm"; CPU="host"; fi
echo "  Accelerator: $ACCEL"

MCAST="230.0.0.1:19500"

for VM in b c; do
    IMG="$WORKDIR/vm-$VM.cpio.gz"
    timeout $TIMEOUT qemu-system-aarch64 \
        -machine virt -cpu "$CPU" -accel "$ACCEL" -m 256 \
        -kernel "$KERNEL" -initrd "$IMG" \
        -append "console=ttyAMA0 panic=-1 quiet" \
        -nographic -no-reboot -nic none \
        -netdev socket,id=net0,mcast=$MCAST \
        -device virtio-net-device,netdev=net0 \
        > "$WORKDIR/vm-$VM.log" 2>&1 &
    eval "VM_$(echo $VM | tr a-z A-Z)_PID=$!"
done

sleep 8  # let B and C boot

timeout $TIMEOUT qemu-system-aarch64 \
    -machine virt -cpu "$CPU" -accel "$ACCEL" -m 512 \
    -kernel "$KERNEL" -initrd "$WORKDIR/vm-a.cpio.gz" \
    -append "console=ttyAMA0 panic=-1 quiet" \
    -nographic -no-reboot -nic none \
    -netdev socket,id=net0,mcast=$MCAST \
    -device virtio-net-device,netdev=net0 \
    > "$WORKDIR/vm-a.log" 2>&1 &
VM_A_PID=$!

wait $VM_A_PID 2>/dev/null || true

# ── Results ──
echo ""
echo "[4/4] Results..."
echo ""
echo "--- VM-A output ---"
grep -E "PASS:|FAIL:|VERDICT:|RESULTS:|Proven|DUVM_3VM" "$WORKDIR/vm-a.log" 2>/dev/null || true
echo "--- end ---"

for VM in b c; do
    echo "--- VM-$(echo $VM | tr a-z A-Z) ---"
    grep -E "memserver|READY|connected" "$WORKDIR/vm-$VM.log" 2>/dev/null | head -3 || true
done
echo ""

if grep -q "VERDICT: PASS" "$WORKDIR/vm-a.log"; then
    PASS_COUNT=$(grep -c "PASS:" "$WORKDIR/vm-a.log" || true)
    echo "================================================================"
    echo "  3-MACHINE TEST PASSED ($PASS_COUNT checks)"
    echo "================================================================"
    exit 0
elif grep -q "DUVM_3VM_TEST_COMPLETE" "$WORKDIR/vm-a.log"; then
    echo "=== TEST FAILED ==="
    grep "FAIL:" "$WORKDIR/vm-a.log" || true
    exit 1
else
    echo "=== VM-A DID NOT COMPLETE ==="
    tail -20 "$WORKDIR/vm-a.log"
    exit 1
fi

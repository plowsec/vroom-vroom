#!/bin/zsh
# Disk test with the host cache out of the way: random reads over a 60 GB
# disk (bigger than macOS can cache here), vroom vs QEMU. bench.py's
# disk tests run from the host cache; this one reads from the SSD.
set -u
# usage: scripts/bench-cold.sh WORKDIR   (the bench.py work dir; needs ~65 GB free)
B=${1:?bench work dir}; S=$B/cold; R=${0:A:h:h}; mkdir -p $S
[ -f $S/big.raw ] || dd if=/dev/urandom of=$S/big.raw bs=8m count=7680 2>/dev/null
k() { ssh -q -i $B/id_bench -p 22722 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 ubuntu@127.0.0.1 "$@"; }
waitssh() { for i in {1..60}; do k true 2>/dev/null && return; sleep 3; done; echo "no ssh"; }
jobs() {
  k 'D=$(lsblk -dnbo NAME,SIZE | awk "\$2>60000000000{print \$1}"); echo disk=$D;
     for j in "randread 4k 1" "randread 4k 32" "randread 64k 1"; do set -- $j;
       sudo fio --name=t --filename=/dev/$D --direct=1 --ioengine=libaio --rw=$1 --bs=$2 --iodepth=$3 --runtime=30 --time_based --randrepeat=0 --output-format=terse --terse-version=3 | awk -F";" -v n="$1-$2-qd$3" "{printf \"%s iops=%s lat_mean_us=%.0f p99_us=%s\n\", n, \$8, \$40, \$30}"; done'
}
cat > $S/vroom.toml <<T
name = "cold"
cpus = 4
memory_mib = 4096
[boot]
type = "kernel"
kernel = "$B/vmlinuz"
initrd = "$B/initrd"
cmdline = "console=ttyAMA0 root=LABEL=cloudimg-rootfs ro"
[[disk]]
path = "$B/root.qcow2"
format = "qcow2"
[[disk]]
path = "$B/seed.iso"
readonly = true
format = "raw"
[[disk]]
path = "big.raw"
format = "raw"
[[net]]
mode = "nat"
port_forwards = [{ host_port = 22722, guest_port = 22 }]
[serial]
type = "none"
T
echo "== vroom"; ( cd $S && $R/target/release/vroom run vroom.toml --state-dir $S/v.state --fresh < /dev/null > vroom.log 2>&1 & ); waitssh; jobs; k 'sudo poweroff' ; sleep 6
echo "== qemu"; /opt/homebrew/bin/qemu-system-aarch64 -accel hvf -M virt -cpu host -smp 4 -m 4096 -display none -serial none -monitor none -nodefaults \
  -kernel $B/vmlinuz -initrd $B/initrd -append "console=ttyAMA0 root=LABEL=cloudimg-rootfs ro" \
  -drive file=$B/root.qcow2,if=none,id=d0,format=qcow2 -device virtio-blk-pci,drive=d0 \
  -drive file=$B/seed.iso,if=none,id=d1,format=raw,readonly=on -device virtio-blk-pci,drive=d1 \
  -drive file=$S/big.raw,if=none,id=d2,format=raw -device virtio-blk-pci,drive=d2 \
  -netdev user,id=n0,hostfwd=tcp:127.0.0.1:22722-:22 -device virtio-net-pci,netdev=n0 > $S/qemu.log 2>&1 &
waitssh; jobs; k 'sudo poweroff'; sleep 6

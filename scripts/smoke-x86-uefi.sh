#!/bin/sh
# Boot a tiny EFI app through OVMF/KVM, NVMe and FAT; no OS image needed.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -m)" = x86_64 ] || { echo "needs x86-64 Linux with /dev/kvm" >&2; exit 1; }
[ -r /dev/kvm ] && [ -w /dev/kvm ] || { echo "needs read/write access to /dev/kvm" >&2; exit 1; }
for tool in clang lld-link sfdisk mformat mmd mcopy timeout; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing $tool" >&2; exit 1; }
done
cpus=${VROOM_SMOKE_CPUS:-2}
case "$cpus" in 1|2|4) ;; *) echo "VROOM_SMOKE_CPUS must be 1, 2 or 4" >&2; exit 1 ;; esac
memory=${VROOM_SMOKE_MEMORY_MIB:-512}
case "$memory" in ''|*[!0-9]*) echo "VROOM_SMOKE_MEMORY_MIB must be an integer" >&2; exit 1 ;; esac
[ "$memory" -ge 64 ] && [ "$memory" -le 457728 ] || { echo "memory must be 64..457728 MiB" >&2; exit 1; }
cargo build -p vroom-cli
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
clang --target=x86_64-pc-win32-coff -ffreestanding -fshort-wchar -mno-red-zone \
  -fno-stack-protector -O2 -DEXPECTED_CPUS="$cpus" \
  -c scripts/guest/uefi-probe.c -o "$tmp/probe.obj"
lld-link /subsystem:efi_application /entry:efi_main /nodefaultlib \
  "/out:$tmp/BOOTX64.EFI" "$tmp/probe.obj"
truncate -s 512M "$tmp/boot.img"
printf 'label: dos\nlabel-id: 0x31545256\nstart=2048, type=c, bootable\n' | sfdisk "$tmp/boot.img" >/dev/null
image=$tmp/boot.img@@1048576
mformat -i "$image" -F -T 1046528 -v UEFIPROBE ::
mmd -i "$image" ::/EFI ::/EFI/BOOT
mcopy -i "$image" "$tmp/BOOTX64.EFI" ::/EFI/BOOT/BOOTX64.EFI
cat > "$tmp/ovmf.toml" <<CONFIG
name = "x64-uefi-probe"
cpus = $cpus
memory_mib = $memory
os = "linux"
[[disk]]
path = "$tmp/boot.img"
bus = "nvme"
format = "raw"
readonly = true
[boot]
type = "uefi"
vars = "$tmp/vars.fd"
[clipboard]
host_to_guest = false
guest_to_host = false
CONFIG
# The VMM's sandbox remains enabled. The caller needs access to host KVM.
if timeout --signal=INT --kill-after=5s 45s target/debug/vroom run "$tmp/ovmf.toml" --fresh > "$tmp/boot.log" 2>&1; then
  cat "$tmp/boot.log"
  grep -q VROOM_X64_UEFI_BOOT_OK "$tmp/boot.log"
else
  cat "$tmp/boot.log" >&2
  exit 1
fi

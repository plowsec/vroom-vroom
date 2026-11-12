#!/bin/sh
# Secure Boot firmware into target/firmware (scripts/app.sh puts it in
# vroom.app; the VMM finds it there), from Debian's EDK2 (pinned,
# scripts/fetch.sh aavmf):
#   AAVMF_CODE.secboot.fd  ArmVirtQemu with SECURE_BOOT_ENABLE and TPM2
#   AAVMF_VARS.ms.fd       NVRAM template: Secure Boot on, Debian's PK,
#                          Microsoft's KEK, db (Windows PCA and UEFI CA
#                          2011) and dbx, plus Microsoft's 2023 KEK and db
#                          CAs (pinned too; scripts/enroll-keys.py): Windows
#                          updated to the 2023 boot manager needs them
#   copyright              EDK2's licenses (BSD-2-Clause-Patent and others)
# Both images are cut to their used part (the rest is zeros); the VMM pads
# them back to the 64 MiB flash.
set -e
cd "$(dirname "$0")/.."
deb=$(scripts/fetch.sh aavmf)
out=target/firmware
mkdir -p "$out"

# name, bytes kept
take() {
  ar p "$deb" data.tar.xz | tar -xOf - "./usr/share$1" >"$out/full.part"
  head -c "$2" "$out/full.part" >"$out/$(basename "$1").part"
  [ "$(tail -c +$(($2 + 1)) "$out/full.part" | tr -d '\0' | wc -c)" -eq 0 ] ||
    { echo "$1: not zeros past $2 bytes" >&2; exit 1; }
  mv "$out/$(basename "$1").part" "$out/$(basename "$1")"
  rm -f "$out/full.part"
}
take /AAVMF/AAVMF_CODE.secboot.fd 3145728
take /AAVMF/AAVMF_VARS.ms.fd 786432
python3 -I scripts/enroll-keys.py "$out/AAVMF_VARS.ms.fd" \
  --db "$(scripts/fetch.sh ms-windows-ca-2023)" \
  --db "$(scripts/fetch.sh ms-uefi-ca-2023)" \
  --db "$(scripts/fetch.sh ms-option-rom-ca-2023)" \
  --kek "$(scripts/fetch.sh ms-kek-ca-2023)" >&2
ar p "$deb" data.tar.xz | tar -xOf - ./usr/share/doc/qemu-efi-aarch64/copyright >"$out/copyright"
echo "$out"

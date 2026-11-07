#!/usr/bin/env python3
"""Read-only compatibility precheck for the installed ARM Linux harness kernel."""
import argparse
import json
from pathlib import Path
import struct

REQUIRED = ('ARM64', 'OF', 'ARM_GIC_V3', 'ARM_ARCH_TIMER', 'ARM_PSCI_FW', 'SERIAL_AMBA_PL011',
            'SERIAL_AMBA_PL011_CONSOLE', 'BLK_DEV_INITRD', 'BINFMT_ELF')


def check(kernel, config):
    if not 64 <= len(kernel) <= 128 * 1024 * 1024:
        raise ValueError('installed kernel is outside the input size bound')
    settings = dict(line.split('=', 1) for line in config.splitlines() if line.startswith('CONFIG_') and '=' in line)
    missing = [name for name in REQUIRED if settings.get('CONFIG_' + name) != 'y']
    if missing:
        raise ValueError('kernel needs built-in features: ' + ', '.join(missing))
    if kernel[:2] == b'\x1f\x8b':
        kind = 'gzip; ARM Image header checked by bounded Rust decompressor'
    elif kernel[:2] == b'MZ' and kernel[4:8] == b'zimg':
        offset, size = struct.unpack_from('<II', kernel, 8)
        compression = kernel[24:32].split(b'\0', 1)[0]
        if offset < 32 or size == 0 or offset + size > len(kernel) or compression not in (b'gzip', b'zstd', b'zstd22'):
            raise ValueError('unsupported or malformed EFI zboot payload')
        kind = 'EFI zboot ' + compression.decode() + '; extracted Image direct boot only'
    elif kernel[56:60] == b'ARM\x64':
        kind = 'ARM64 Image'
    else:
        raise ValueError('kernel is not a supported ARM Image, gzip or EFI zboot wrapper')
    return {'compatible_precheck': True, 'kernel_format': kind, 'firmware_boot_verified': False,
            'guest_execution_verified': False, 'required_builtins': list(REQUIRED)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('kernel', type=Path)
    parser.add_argument('config', type=Path)
    args = parser.parse_args()
    try:
        if not args.kernel.is_file() or args.kernel.stat().st_size > 128 * 1024 * 1024:
            raise ValueError('kernel is not a bounded regular file')
        if not args.config.is_file() or args.config.stat().st_size > 1024 * 1024:
            raise ValueError('kernel config is not a bounded regular file')
        with args.kernel.open('rb') as stream:
            kernel = stream.read(128 * 1024 * 1024 + 1)
        with args.config.open('rb') as stream:
            config = stream.read(1024 * 1024 + 1)
        if len(config) > 1024 * 1024:
            raise ValueError('kernel config grew beyond its bound')
        print(json.dumps(check(kernel, config.decode())))
    except (OSError, ValueError) as error:
        parser.exit(2, f'ARM kernel precheck: {error}\n')


if __name__ == '__main__':
    main()

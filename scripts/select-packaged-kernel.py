#!/usr/bin/env python3
"""Select the exact installed, RPM-digested AL2023 kernel. Read-only; never downloads."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import stat
import subprocess

OWNERS = {'kernel', 'kernel-core', 'kernel6.18', 'kernel6.18-core'}
HEADER = '%{NAME}\t%{VERSION}\t%{RELEASE}\t%{ARCH}\t%{VENDOR}\t%{FILEDIGESTALGO}\n'
INVENTORY = '[%{FILENAMES}\t%{FILEMODES}\t%{FILEFLAGS}\t%{FILEDIGESTS}\n]'
GHOST = 64


class KernelError(ValueError):
    pass


def rpm(*arguments):
    result = subprocess.run(['/usr/bin/rpm', *arguments], capture_output=True, text=True, timeout=5,
                            env={'PATH': '/usr/sbin:/usr/bin:/sbin:/bin', 'LC_ALL': 'C'})
    if result.returncode or len(result.stdout) > 16 * 1024 * 1024:
        raise KernelError('RPM query failed or exceeded its output bound')
    return result.stdout


def metadata(text, release, architecture):
    lines = text.splitlines()
    if len(lines) != 1 or len(lines[0].split('\t')) != 6:
        raise KernelError('ambiguous or malformed RPM owner metadata')
    name, version, package_release, arch, vendor, algorithm = lines[0].split('\t')
    if name not in OWNERS or vendor != 'Amazon Linux' or arch != architecture:
        raise KernelError('kernel owner, vendor or architecture differs from the reviewed profile')
    if version + '-' + package_release + '.' + arch != release or '.amzn2023' not in package_release:
        raise KernelError('RPM version-release does not match the running AL2023 kernel')
    if name.startswith('kernel6.18') and not version.startswith('6.18.'):
        raise KernelError('versioned kernel package is not the reviewed 6.18 series')
    if algorithm != '8':
        raise KernelError('kernel package must record SHA256 file digests (algorithm 8)')
    return {'name': name, 'version': version, 'release': package_release, 'architecture': arch, 'vendor': vendor,
            'nevra': f'{name}-{version}-{package_release}.{arch}', 'digest_algorithm': 8}


def inventory(text):
    records = []
    seen = set()
    for line in text.splitlines():
        fields = line.split('\t')
        if len(fields) != 4:
            raise KernelError('malformed RPM file inventory')
        name, mode, flags, digest = fields
        path = PurePosixPath(name)
        if not path.is_absolute() or '..' in path.parts or name in seen:
            raise KernelError('unsafe or duplicate RPM inventory path')
        try:
            records.append({'path': name, 'mode': int(mode), 'flags': int(flags), 'sha256': digest})
        except ValueError as error:
            raise KernelError('invalid RPM file mode or flags') from error
        seen.add(name)
    return records


def regular_path(root, name):
    """Only the conventional top-level /lib -> /usr/lib alias is permitted."""
    parts = PurePosixPath(name).parts[1:]
    if parts and parts[0] == 'lib' and (root / 'lib').is_symlink():
        if os.readlink(root / 'lib') not in ('usr/lib', '/usr/lib'):
            raise KernelError('unexpected /lib symlink target')
        parts = ('usr', 'lib', *parts[1:])
    current = root
    for part in parts:
        current = current / part
        if current.is_symlink():
            raise KernelError('kernel payload has an unexpected path symlink')
    if not current.is_file():
        raise KernelError('RPM payload is missing or not a regular file')
    return current


def select(records, allowed, root, limit):
    matches = []
    for record in records:
        if record['path'] not in allowed:
            continue
        if (not stat.S_ISREG(record['mode']) or record['flags'] & GHOST
                or re.fullmatch('[0-9a-fA-F]{64}', record['sha256']) is None):
            raise KernelError('candidate payload is ghost/generated or lacks a regular-file SHA256 digest')
        path = regular_path(root, record['path'])
        matches.append((record, path))
    if len(matches) != 1:
        raise KernelError('expected one unambiguous RPM-owned payload with a recorded SHA256')
    record, path = matches[0]
    digest, size = hashlib.sha256(), 0
    with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW), 'rb') as stream:
        info = os.fstat(stream.fileno())
        if not stat.S_ISREG(info.st_mode) or not 0 < info.st_size <= limit:
            raise KernelError('payload is not a bounded regular file')
        while chunk := stream.read(1024 * 1024):
            size += len(chunk)
            if size > limit:
                raise KernelError('payload grew beyond its size limit')
            digest.update(chunk)
    actual = digest.hexdigest()
    if actual != record['sha256'].lower():
        raise KernelError('installed payload SHA256 differs from the RPM database')
    return {'path': '/' + path.relative_to(root).as_posix(), 'rpm_path': record['path'],
            'sha256': actual, 'bytes': size, 'rpm_digest_verified': True}


def resolve(release, architecture, require_config=False, root=Path('/'), query=rpm):
    root = root.resolve()
    if architecture not in ('aarch64', 'x86_64') or re.fullmatch(r'[0-9][A-Za-z0-9._+-]{0,199}', release) is None:
        raise KernelError('unsupported host architecture or kernel release')
    boot = '/boot/vmlinuz-' + release
    # The generated boot copy is only an owner lookup; its contents/digest
    # never establish payload provenance.
    owner = metadata(query('-qf', '--queryformat', HEADER, boot), release, architecture)
    records = inventory(query('-q', '--queryformat', INVENTORY, owner['nevra']))
    paths = {'/lib/modules/' + release + '/vmlinuz', '/usr/lib/modules/' + release + '/vmlinuz'}
    kernel = select(records, paths, root, 128 * 1024 * 1024)
    if metadata(query('-qf', '--queryformat', HEADER, kernel['rpm_path']), release, architecture) != owner:
        raise KernelError('canonical payload owner differs from the discovered package')
    config = None
    if require_config:
        config = select(records, {'/lib/modules/' + release + '/config',
                                  '/usr/lib/modules/' + release + '/config'}, root, 1024 * 1024)
        if metadata(query('-qf', '--queryformat', HEADER, config['rpm_path']), release, architecture) != owner:
            raise KernelError('kernel config owner differs from the discovered package')
    return {'schema_version': 1, 'kernel_release': release, 'architecture': architecture, 'owner': owner,
            'kernel': kernel, 'config': config, 'guest_execution_verified': False,
            'provenance_scope': 'installed RPM owner metadata and recorded file digest; package signature not reverified here'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--architecture', choices=('arm64', 'x86_64'), required=True)
    parser.add_argument('--require-config', action='store_true')
    args = parser.parse_args()
    try:
        expected = 'aarch64' if args.architecture == 'arm64' else 'x86_64'
        uname = os.uname()
        if uname.sysname != 'Linux' or uname.machine != expected:
            raise KernelError('host architecture differs from requested kernel profile')
        print(json.dumps(resolve(uname.release, expected, args.require_config)))
    except (OSError, ValueError, subprocess.TimeoutExpired) as error:
        parser.exit(2, f'packaged kernel: {error}\n')


if __name__ == '__main__':
    main()

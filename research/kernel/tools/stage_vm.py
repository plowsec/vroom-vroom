#!/usr/bin/env python3
"""Stage a powered-off VMware bundle onto an internal disk, read-only at source.

Copies regular files, excluding locks/logs/scoreboards. Refuses symlinks and a
destination on /Volumes. Publishes STAGING_COMPLETE.json only after destination
hash verification and a second source metadata/running-VM check. An interrupted
copy is deliberately retained without a completion marker for inspection.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import time

GIB = 1024 ** 3
VMRUN = Path('/Applications/VMware Fusion.app/Contents/Library/vmrun')


def stopped(source):
    result = subprocess.run([str(VMRUN), 'list'], check=True, capture_output=True, text=True)
    for line in result.stdout.splitlines()[1:]:
        if Path(line).resolve().parent == source:
            raise RuntimeError('source VM is running; no copy performed')


def signature(path):
    s = path.stat(follow_symlinks=False)
    return [s.st_dev, s.st_ino, s.st_size, s.st_mtime_ns, s.st_ctime_ns]


def digest(path):
    h = hashlib.sha256()
    with path.open('rb') as f:
        for chunk in iter(lambda: f.read(8 * 1024 ** 2), b''):
            h.update(chunk)
    return h.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('source', type=Path)
    parser.add_argument('destination', type=Path)
    parser.add_argument('--copy', action='store_true', help='otherwise only report the plan')
    args = parser.parse_args()
    source = args.source.resolve(strict=True)
    destination = args.destination.resolve()
    if source.suffix != '.vmwarevm' or not source.is_dir():
        raise RuntimeError('source must be a VMware bundle')
    if destination.is_relative_to('/Volumes') or destination.is_relative_to(source):
        raise RuntimeError('destination must be outside /Volumes and the source bundle')
    if destination.exists() or not destination.parent.is_dir():
        raise RuntimeError('destination must be new and its parent must already exist')
    stopped(source)
    files = []
    for p in sorted(source.iterdir()):
        if p.is_symlink():
            raise RuntimeError(f'symlink in source: {p.name}')
        if p.name.endswith(('.lck', '.log', '.scoreboard')):
            continue
        if not p.is_file():
            raise RuntimeError(f'unexpected directory in source: {p.name}')
        files.append(p)
    before = {p.name: signature(p) for p in files}
    required = sum(s[2] for s in before.values())
    free = shutil.disk_usage(destination.parent).free
    # Reserve another full copy for qcow2 conversion plus host headroom.
    if free < required * 2 + 32 * GIB:
        raise RuntimeError('insufficient space for staging, conversion and 32 GiB reserve')
    print(json.dumps({'source': str(source), 'destination': str(destination),
                      'files': len(files), 'copy_gib': round(required / GIB, 2),
                      'free_gib': round(free / GIB, 2)}, indent=2), flush=True)
    if not args.copy:
        return
    destination.mkdir(mode=0o700)
    manifest = {'source': str(source), 'files': {}, 'started': time.time()}
    for p in files:
        out = destination / p.name
        if shutil.disk_usage(destination).free < p.stat().st_size + 32 * GIB:
            raise RuntimeError('space reserve reached; staging incomplete')
        h = hashlib.sha256()
        with p.open('rb') as src, out.open('xb') as dst:
            os.chmod(out, 0o600)
            for chunk in iter(lambda: src.read(8 * 1024 ** 2), b''):
                h.update(chunk)
                dst.write(chunk)
            dst.flush()
            os.fsync(dst.fileno())
        if signature(p) != before[p.name] or digest(out) != h.hexdigest():
            raise RuntimeError(f'copy verification failed: {p.name}')
        manifest['files'][p.name] = {'sha256': h.hexdigest(), 'stat': before[p.name]}
        if p.stat().st_size > GIB:
            print(f'verified {p.name}', flush=True)
    stopped(source)
    if any(signature(p) != before[p.name] for p in files):
        raise RuntimeError('source changed during staging; do not import this copy')
    # Catch creation/deletion of disk/config files during the copy too.
    current = {p.name for p in source.iterdir()
               if not p.name.endswith(('.lck', '.log', '.scoreboard'))}
    if current != set(before):
        raise RuntimeError('source file set changed during staging')
    manifest['completed'] = time.time()
    with (destination / 'STAGING_COMPLETE.json').open('x') as f:
        json.dump(manifest, f, indent=2)
    print('STAGING_COMPLETE: source unchanged; every copied file verified', flush=True)


if __name__ == '__main__':
    main()

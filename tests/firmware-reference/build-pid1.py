#!/usr/bin/env python3
"""Build only the static PID1, offline, and record fresh local provenance.

Does not run PID1, a TPM helper or a guest. Requires an installed x64 musl target
and cached locked dependencies; it never installs missing inputs.
"""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import subprocess
import inputs


def build(output, cargo, rustc, linker, llvm_library):
    output = inputs.output_path(output)
    if output.exists():
        raise ValueError('build output must be new')
    tools = {name: inputs.file_path(str(path), True)
             for name, path in [('cargo', cargo), ('rustc', rustc), ('linker', linker)]}
    tools['llvm_library'] = inputs.file_path(str(llvm_library))
    sources = inputs.pid1_sources()
    env = {key: value for key, value in os.environ.items()
           if key not in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'RUSTC_WRAPPER',
                          'RUSTC_WORKSPACE_WRAPPER')}
    env.update(RUSTC=str(tools['rustc']), CARGO_NET_OFFLINE='true',
               CARGO_TARGET_DIR=str(output / 'target'),
               CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=str(tools['linker']),
               DYLD_LIBRARY_PATH=str(tools['llvm_library'].parent))
    command = [str(tools['cargo']), 'build', '--locked', '--offline', '--release',
               '--target', 'x86_64-unknown-linux-musl', '--manifest-path',
               str(inputs.BASE / 'pid1/Cargo.toml')]
    versions = {name: subprocess.check_output([str(tools[name]), flag], text=True,
                 stderr=subprocess.STDOUT, timeout=10).strip()
                for name, flag in [('cargo', '--version'), ('rustc', '-vV')]}
    target_libdir = Path(subprocess.check_output(
        [str(tools['rustc']), '--print', 'target-libdir', '--target', 'x86_64-unknown-linux-musl'],
        text=True, stderr=subprocess.STDOUT, timeout=10).strip())
    target_libraries = {str(path.resolve()): inputs.digest(path)
                        for path in sorted(target_libdir.rglob('*')) if path.is_file()}
    if not target_libraries:
        raise ValueError('installed x64 musl standard libraries are required')
    os.umask(0o077)
    output.mkdir(parents=True, mode=0o700)
    completed = subprocess.run(command, cwd=inputs.BASE / 'pid1', env=env,
                               stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, timeout=180)
    (output / 'build.log').write_bytes(completed.stdout)
    if completed.returncode:
        raise RuntimeError('offline PID1 build failed; inspect ignored build.log')
    if inputs.pid1_sources() != sources:
        raise ValueError('PID1 source changed during build')
    binary = output / 'target/x86_64-unknown-linux-musl/release/vroom-tpm-reference-init'
    if not binary.is_file():
        raise ValueError('static PID1 binary missing')
    import prepare
    prepare.static_x64(binary)
    manifest = {'schema': 1, 'scope': 'local_qemu_firmware_tpm_reference_pid1',
                'source_sha256': sources,
                'binary': {'path': str(binary), 'sha256': inputs.digest(binary),
                           'bytes': binary.stat().st_size},
                'tools': {name: {'path': str(path), 'sha256': inputs.digest(path)}
                          for name, path in tools.items()},
                'versions': versions, 'command': command,
                'target_library_sha256': target_libraries,
                'build_script_sha256': inputs.digest(Path(__file__)),
                'inputs_script_sha256': inputs.digest(inputs.BASE / 'inputs.py'),
                'prepare_script_sha256': inputs.digest(inputs.BASE / 'prepare.py'),
                'build_log_sha256': inputs.digest(output / 'build.log'),
                'guest_or_helper_executed': False}
    path = output / 'pid1-build.json'
    path.write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n')
    return {'manifest': str(path), 'manifest_sha256': inputs.digest(path),
            'pid1_sha256': manifest['binary']['sha256'], 'guest_or_helper_executed': False}


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--cargo', type=Path, required=True)
    parser.add_argument('--rustc', type=Path, required=True)
    parser.add_argument('--linker', type=Path, required=True)
    parser.add_argument('--llvm-library', type=Path, required=True)
    args = parser.parse_args()
    print(json.dumps(build(args.output, args.cargo, args.rustc, args.linker, args.llvm_library), sort_keys=True))

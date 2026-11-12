"""Explicit local inputs and ignored output locations for the reference fixture."""
from __future__ import annotations
import hashlib
import json
import os
from pathlib import Path

BASE = Path(__file__).resolve().parent
ROOT = BASE.parents[1]
OUTPUT_ROOTS = (ROOT / '.aws-pilot', BASE / 'output')
EXECUTABLES = ('qemu', 'swtpm', 'openssl', 'osslsigncode', 'clang', 'lld_link',
               'mformat', 'mcopy', 'mmd')
PATH_KEYS = (*EXECUTABLES, 'llvm_library', 'firmware_code', 'firmware_vars', 'kernel')


def digest(path):
    with open(path, 'rb') as stream:
        return hashlib.file_digest(stream, 'sha256').hexdigest()


def file_path(value, executable=False):
    if not isinstance(value, str) or not Path(value).is_absolute():
        raise ValueError('input paths must be explicit and absolute')
    path = Path(value).resolve(strict=True)
    if not path.is_file() or any(c in str(path) for c in ',\r\n\0'):
        raise ValueError('input must be a regular file with an unambiguous path')
    if executable and not os.access(path, os.X_OK):
        raise ValueError('input tool is not executable')
    return path


def validate_paths(value):
    if not isinstance(value, dict) or set(value) != set(PATH_KEYS):
        raise ValueError('exact named tool, kernel and firmware inputs are required')
    return {key: file_path(value[key], key in EXECUTABLES) for key in PATH_KEYS}


def load(path):
    value = json.loads(Path(path).read_text())
    if not isinstance(value, dict) or set(value) != {'paths', 'kernel_sha256'}:
        raise ValueError('inputs require paths and kernel_sha256 only')
    sha = value['kernel_sha256']
    if not isinstance(sha, str) or len(sha) != 64 or any(c not in '0123456789abcdef' for c in sha):
        raise ValueError('expected kernel SHA256 is required')
    return validate_paths(value['paths']), sha


def output_path(value):
    """Resolve existing symlinks before allowing writes beneath ignored roots."""
    path = Path(value).resolve()
    if any(c in str(path) for c in ',\r\n\0'):
        raise ValueError('output requires an unambiguous path')
    if not any(path != root.resolve() and path.is_relative_to(root.resolve())
               for root in OUTPUT_ROOTS):
        raise ValueError('outputs must be beneath .aws-pilot/ or fixture output/')
    return path


def pid1_sources():
    source = BASE / 'pid1'
    names = ['Cargo.toml', 'Cargo.lock', '.cargo/config.toml']
    names += [str(p.relative_to(source)) for p in sorted((source / 'src').glob('*.rs'))]
    names += [str(p.relative_to(source)) for p in sorted((source / 'tests').glob('*.rs'))]
    return {name: digest(source / name) for name in names}

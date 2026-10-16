"""Explicit RVA analysis copies for angr's signed-address limitations.

The original runtime export stays intact. Only DIR64 relocation sites whose
current value still points into this module are rebased. External/live object
pointers remain runtime values. This is never a runnable PE.
"""
import json
import struct
from pathlib import Path


def prepare_rva_image(exported, *, allow_unrelocated=False):
    source = Path(exported['path'])
    data = bytearray(source.read_bytes())
    base = exported['image']['base']
    size = exported['image']['size']
    missing = set(exported['missing_page_rvas'])
    if len(data) != size or size < 4096:
        raise ValueError('Analysis export size mismatch')

    def readable(offset, length):
        if offset < 0 or length < 0 or offset + length > size:
            raise ValueError('Analysis metadata points outside the exported image')
        if length and any(page in missing for page in range(offset & ~4095, (offset + length - 1 & ~4095) + 4096, 4096)):
            raise ValueError('Analysis relocation metadata or target is unreadable')

    pe, = struct.unpack_from('<I', data, 0x3c)
    readable(pe, 264)
    if data[:2] != b'MZ' or data[pe:pe+4] != b'PE\0\0' or struct.unpack_from('<H', data, pe+4)[0] != 0xaa64:
        raise ValueError('Expected reconstructed ARM64 PE')
    if struct.unpack_from('<Q', data, pe+48)[0] != base:
        raise ValueError('Runtime image base does not match its manifest')
    start, length = exported['image']['relocation_directory']
    relocation_warning = None
    try:
        readable(start, length)
    except ValueError:
        if not allow_unrelocated or start < 0 or length < 0 or start + length > size:
            raise
        relocation_warning = 'Relocations are unreadable: code uses RVAs, but absolute internal pointers remain runtime addresses and cannot be resolved in this view.'
        length = 0
    cursor, end = start, start + length
    relocated, retained, unavailable = 0, 0, 0
    while cursor < end:
        if cursor + 8 > end:
            raise ValueError('Truncated relocation block')
        page, block_size = struct.unpack_from('<II', data, cursor)
        if block_size < 8 or block_size % 2 or cursor + block_size > end:
            raise ValueError('Invalid relocation block size')
        for offset in range(cursor+8, cursor+block_size, 2):
            entry, = struct.unpack_from('<H', data, offset)
            kind, target = entry >> 12, page + (entry & 4095)
            if kind == 0:
                continue
            if kind != 10:
                raise ValueError(f'Unsupported ARM64 relocation type {kind}')
            if target + 8 > size:
                raise ValueError('Relocation target outside image')
            if target // 4096 * 4096 in missing or (target+7) // 4096 * 4096 in missing:
                unavailable += 1
                continue
            value, = struct.unpack_from('<Q', data, target)
            if base <= value < base + size:
                struct.pack_into('<Q', data, target, value-base)
                relocated += 1
            else:
                retained += 1
        cursor += block_size
    struct.pack_into('<Q', data, pe+48, 0)
    destination = source.with_suffix('.rva.exe')
    with destination.open('xb') as stream:
        stream.write(data)
    provenance = {
        'source': str(source), 'runtime_base': base, 'analysis_base': 0,
        'relocated_internal_pointers': relocated, 'retained_other_pointers': retained,
        'unavailable_relocations': unavailable, 'missing_page_rvas': sorted(missing),
        'warning': relocation_warning,
        'purpose': 'analysis only; code addresses are RVAs; external pointers retain runtime values',
    }
    with destination.with_suffix('.json').open('x') as stream:
        json.dump(provenance, stream, indent=2)
    return destination, provenance

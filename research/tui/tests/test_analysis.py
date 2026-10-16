import json
import struct

import pytest

from vroom_tui.analysis import prepare_rva_image


def fixture(tmp_path):
    base = 0xfffff80010000000
    data = bytearray(0x3000)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 0x3c, 0x80)
    data[0x80:0x84] = b'PE\0\0'
    struct.pack_into('<H', data, 0x84, 0xaa64)
    struct.pack_into('<Q', data, 0xb0, base)
    struct.pack_into('<IIHH', data, 0x2000, 0x1000, 12, 0xa000, 0xa008)
    struct.pack_into('<QQ', data, 0x1000, base+0x1234, 0xffff800000008000)
    path = tmp_path / 'image.exe'
    path.write_bytes(data)
    return {'path': str(path), 'image': {'base': base, 'size': len(data), 'relocation_directory': [0x2000, 12]}, 'missing_page_rvas': []}


def test_rebases_internal_pointers_and_preserves_external_values(tmp_path):
    exported = fixture(tmp_path)
    result, _ = prepare_rva_image(exported)
    data = result.read_bytes()
    assert struct.unpack_from('<Q', data, 0xb0)[0] == 0
    assert struct.unpack_from('<QQ', data, 0x1000) == (0x1234, 0xffff800000008000)
    metadata = json.loads(result.with_suffix('.json').read_text())
    assert metadata['relocated_internal_pointers'] == 1
    assert metadata['retained_other_pointers'] == 1
    assert struct.unpack_from('<Q', (tmp_path/'image.exe').read_bytes(), 0xb0)[0] == exported['image']['base']


def test_missing_relocation_table_is_not_treated_as_empty(tmp_path):
    exported = fixture(tmp_path)
    exported['missing_page_rvas'] = [0x2000]
    with pytest.raises(ValueError, match='unreadable'):
        prepare_rva_image(exported)
    result, metadata = prepare_rva_image(exported, allow_unrelocated=True)
    assert 'cannot be resolved' in metadata['warning']
    assert struct.unpack_from('<Q', result.read_bytes(), 0x1000)[0] == exported['image']['base'] + 0x1234


def test_invalid_relocation_block_is_rejected(tmp_path):
    exported = fixture(tmp_path)
    path = tmp_path/'image.exe'
    data = bytearray(path.read_bytes())
    struct.pack_into('<I', data, 0x2004, 0)
    path.write_bytes(data)
    with pytest.raises(ValueError, match='block size'):
        prepare_rva_image(exported)

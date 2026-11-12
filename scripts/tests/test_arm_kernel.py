import importlib.util
from pathlib import Path
import struct
import unittest

SPEC = importlib.util.spec_from_file_location('arm_kernel', Path(__file__).parents[1] / 'check-arm-kernel.py')
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)
CONFIG = '\n'.join('CONFIG_' + name + '=y' for name in MODULE.REQUIRED)


class ArmKernelTests(unittest.TestCase):
    def image(self):
        image = bytearray(64); image[56:60] = b'ARM\x64'; return image

    def test_builtin_console_is_required_and_modular_is_insufficient(self):
        self.assertTrue(MODULE.check(self.image(), CONFIG)['compatible_precheck'])
        for value in ('m', 'n'):
            with self.assertRaisesRegex(ValueError, 'built-in'):
                MODULE.check(self.image(), CONFIG.replace('CONFIG_SERIAL_AMBA_PL011=y', 'CONFIG_SERIAL_AMBA_PL011=' + value))

    def test_wrong_architecture_and_malformed_zboot_are_rejected(self):
        with self.assertRaises(ValueError): MODULE.check(bytes(64), CONFIG)
        image = self.image(); image[:8] = b'MZ\0\0zimg'; image[24:32] = b'gzip\0\0\0\0'
        for offset, size in ((0, 1), (32, 0), (32, 33), (0xffffffff, 64)):
            struct.pack_into('<II', image, 8, offset, size)
            with self.assertRaises(ValueError): MODULE.check(image, CONFIG)
        struct.pack_into('<II', image, 8, 32, 32)
        result = MODULE.check(image, CONFIG)
        self.assertFalse(result['guest_execution_verified'])
        self.assertFalse(result['firmware_boot_verified'])
        image[24:32] = b'unknown\0'
        with self.assertRaises(ValueError): MODULE.check(image, CONFIG)


if __name__ == '__main__': unittest.main()

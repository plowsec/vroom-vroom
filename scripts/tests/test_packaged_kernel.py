import hashlib
import importlib.util
from pathlib import Path
import stat
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('packaged_kernel', Path(__file__).parents[1] / 'select-packaged-kernel.py')
kernel = importlib.util.module_from_spec(spec)
spec.loader.exec_module(kernel)
RELEASE = '6.18.56-125.164.amzn2023.x86_64'
HEADER = 'kernel6.18\t6.18.56\t125.164.amzn2023\tx86_64\tAmazon Linux\t8\n'


class PackageFixture:
    def __init__(self, root):
        self.root = root
        self.header = HEADER
        self.canonical = '/lib/modules/' + RELEASE + '/vmlinuz'
        self.config = '/lib/modules/' + RELEASE + '/config'
        self.records = []
        self.payload(self.canonical, b'packaged kernel')
        self.payload(self.config, b'CONFIG_ARM64=y\n')
        # Official AL2023 /boot files are generated and lack RPM digests.
        self.records.append(('/boot/vmlinuz-' + RELEASE, stat.S_IFREG | 0o644, 64, ''))
        self.records.append(('/boot/config-' + RELEASE, stat.S_IFREG | 0o644, 64, ''))
        self.owner_override = {}
        self.calls = []

    def payload(self, name, data):
        path = self.root / name[1:]
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        self.records.append((name, stat.S_IFREG | 0o644, 0, hashlib.sha256(data).hexdigest()))

    def query(self, *arguments):
        self.calls.append(arguments)
        if arguments[0] == '-qf':
            return self.owner_override.get(arguments[-1], self.header)
        if arguments == ('-q', '--queryformat', kernel.INVENTORY,
                         'kernel6.18-6.18.56-125.164.amzn2023.x86_64'):
            return ''.join('\t'.join(map(str, record)) + '\n' for record in self.records)
        raise AssertionError(arguments)

    def resolve(self, **kwargs):
        return kernel.resolve(RELEASE, 'x86_64', root=self.root, query=self.query, **kwargs)


class PackagedKernelTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.fixture = PackageFixture(self.root)

    def test_canonical_payload_and_config_ignore_generated_boot_copies(self):
        result = self.fixture.resolve(require_config=True)
        self.assertEqual(result['kernel']['rpm_path'], self.fixture.canonical)
        self.assertEqual(result['config']['rpm_path'], self.fixture.config)
        self.assertTrue(result['kernel']['rpm_digest_verified'])
        self.assertFalse(result['guest_execution_verified'])
        self.assertIn('signature not reverified', result['provenance_scope'])
        self.assertEqual(len(self.fixture.calls), 4)
        self.assertFalse((self.root / 'boot').exists())

    def test_standard_lib_alias_is_normalized_and_payload_owner_rechecked(self):
        (self.root / 'usr').mkdir()
        (self.root / 'lib').rename(self.root / 'usr/lib')
        (self.root / 'lib').symlink_to('usr/lib')
        result = self.fixture.resolve()
        self.assertEqual(result['kernel']['path'], '/usr' + self.fixture.canonical)
        self.assertEqual(result['kernel']['rpm_path'], self.fixture.canonical)
        self.fixture.owner_override[self.fixture.canonical] = HEADER.replace('kernel6.18', 'kernel-core')
        with self.assertRaisesRegex(kernel.KernelError, 'owner differs'):
            self.fixture.resolve()

    def test_ghost_missing_and_malformed_digest_cannot_establish_provenance(self):
        record = self.fixture.records[0]
        for changed in ((record[0], record[1], 64, record[3]),
                        (*record[:3], ''), (*record[:3], '(none)'),
                        (record[0], stat.S_IFLNK | 0o777, 0, record[3])):
            with self.subTest(changed=changed):
                self.fixture.records[0] = changed
                with self.assertRaises(kernel.KernelError):
                    self.fixture.resolve()
        self.fixture.records = self.fixture.records[1:]
        with self.assertRaisesRegex(kernel.KernelError, 'unambiguous'):
            self.fixture.resolve()

    def test_tampering_and_size_bound_are_rejected(self):
        path = self.root / self.fixture.canonical[1:]
        path.write_bytes(b'changed kernel')
        with self.assertRaisesRegex(kernel.KernelError, 'SHA256 differs'):
            self.fixture.resolve()
        with self.assertRaisesRegex(kernel.KernelError, 'bounded regular'):
            kernel.select(kernel.inventory(self.fixture.query('-q', '--queryformat', kernel.INVENTORY,
                          'kernel6.18-6.18.56-125.164.amzn2023.x86_64')),
                          {self.fixture.canonical}, self.root, 1)

    def test_ambiguous_canonical_candidates_are_rejected(self):
        self.fixture.payload('/usr' + self.fixture.canonical, b'packaged kernel')
        with self.assertRaisesRegex(kernel.KernelError, 'unambiguous'):
            self.fixture.resolve()

    def test_unlisted_owner_wrong_vendor_arch_version_and_digest_algorithm_rejected(self):
        for replacement in (HEADER.replace('kernel6.18\t', 'kernel-debug\t'),
                            HEADER.replace('Amazon Linux', 'Other'),
                            HEADER.replace('x86_64', 'aarch64'),
                            HEADER.replace('6.18.56', '6.18.57'),
                            HEADER.replace('\t8\n', '\t1\n'),
                            HEADER + HEADER):
            with self.subTest(replacement=replacement):
                self.fixture.header = replacement
                with self.assertRaises(kernel.KernelError):
                    self.fixture.resolve()

    def test_payload_symlink_and_unexpected_lib_alias_rejected(self):
        path = self.root / self.fixture.canonical[1:]
        path.unlink()
        path.symlink_to(self.root / self.fixture.config[1:])
        with self.assertRaisesRegex(kernel.KernelError, 'symlink'):
            self.fixture.resolve()
        path.unlink()
        (self.root / 'lib').rename(self.root / 'other')
        (self.root / 'lib').symlink_to('other')
        with self.assertRaisesRegex(kernel.KernelError, 'unexpected /lib'):
            self.fixture.resolve()

    def test_corrupt_required_config_cannot_pass_with_valid_kernel(self):
        (self.root / self.fixture.config[1:]).write_bytes(b'corrupt')
        with self.assertRaisesRegex(kernel.KernelError, 'SHA256 differs'):
            self.fixture.resolve(require_config=True)
        self.assertTrue(self.fixture.resolve()['kernel']['rpm_digest_verified'])

    def test_invalid_inventory_and_release_paths_are_rejected(self):
        for value in ('relative\t33188\t0\t' + '0' * 64,
                      '/lib/../outside\t33188\t0\t' + '0' * 64,
                      '/file\t33188\t0\t' + '0' * 64 + '\n/file\t33188\t0\t' + '0' * 64):
            with self.subTest(value=value), self.assertRaises(kernel.KernelError):
                kernel.inventory(value)
        with self.assertRaises(kernel.KernelError):
            kernel.resolve('../../etc/passwd', 'x86_64', root=self.root, query=self.fixture.query)
        self.assertEqual(self.fixture.calls, [])


if __name__ == '__main__':
    unittest.main()

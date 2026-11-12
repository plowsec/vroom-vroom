"""Pure packaging checks; no compiler, TPM helper or guest is started."""
import importlib.util
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock

BASE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(BASE))
import inputs
import prepare
spec = importlib.util.spec_from_file_location('fixture_runner', BASE / 'run.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)


class Inputs(unittest.TestCase):
    def setUp(self):
        (BASE / 'output').mkdir(exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=BASE / 'output')
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.file = self.root / 'input'
        self.file.write_bytes(b'fixture')
        self.file.chmod(0o700)
        self.paths = {key: str(self.file) for key in inputs.PATH_KEYS}

    def test_exact_input_names_and_expected_kernel_digest(self):
        config = self.root / 'inputs.json'
        valid = {'paths': self.paths, 'kernel_sha256': inputs.digest(self.file)}
        config.write_text(json.dumps(valid))
        paths, sha = inputs.load(config)
        self.assertEqual(paths['kernel'], self.file.resolve())
        self.assertEqual(sha, inputs.digest(self.file))
        for value in ({**valid, 'unexpected': True}, {**valid, 'kernel_sha256': ''},
                      {**valid, 'paths': {key: value for key, value in self.paths.items() if key != 'qemu'}}):
            config.write_text(json.dumps(value))
            with self.assertRaises(ValueError): inputs.load(config)

    def test_reject_implicit_and_ambiguous_paths(self):
        for value in ('qemu', '/dev/null', str(self.root)):
            with self.subTest(value=value), self.assertRaises(ValueError): inputs.file_path(value)
        bad = self.root / 'bad,path'; bad.write_bytes(b'fixture')
        with self.assertRaises(ValueError): inputs.file_path(str(bad))

    def test_output_containment_resolves_symlinks_and_rejects_option_delimiters(self):
        self.assertEqual(inputs.output_path(self.root / 'new'), self.root.resolve() / 'new')
        with self.assertRaises(ValueError): inputs.output_path(BASE / 'source.json')
        link = self.root / 'escape'; link.symlink_to(BASE, target_is_directory=True)
        with self.assertRaises(ValueError): inputs.output_path(link / 'leak.json')
        for name in ('bad,path', 'bad\npath', 'bad\rpath'):
            with self.subTest(name=name), self.assertRaises(ValueError): inputs.output_path(self.root / name)

    def test_wrong_source_provenance_rejected_before_output_or_tools(self):
        provenance = self.root / 'pid1-build.json'
        provenance.write_text(json.dumps({'schema': 1, 'scope': 'local_qemu_firmware_tpm_reference_pid1',
                                         'source_sha256': {}}))
        output = self.root / 'candidate'
        with mock.patch.object(prepare, 'run') as launch:
            with self.assertRaisesRegex(ValueError, 'source provenance'):
                prepare.prepare(output, provenance, self.paths, inputs.digest(self.file))
            launch.assert_not_called()
        self.assertFalse(output.exists())

    def test_manifest_paths_must_be_hash_inventoried(self):
        candidate = self.root / 'candidate'; candidate.mkdir()
        manifest = candidate / 'manifest.json'
        value = {'scope': 'local_qemu_firmware_tpm_reference_only', 'paths': self.paths,
                 'inputs': {}, 'sources': {}, 'files': {}, 'boot_count': 3,
                 'per_boot_seconds': 180, 'campaign_seconds': 600, 'cpus': 1, 'ram_mib': 1024,
                 'private_signing_keys_deleted': True, 'token': 'a' * 32}
        manifest.write_text(json.dumps(value))
        with self.assertRaisesRegex(runner.Failed, 'input inventory'):
            runner.verify(candidate, inputs.digest(manifest))
        value['inputs'] = {str(self.file.resolve()): inputs.digest(self.file)}
        manifest.write_text(json.dumps(value))
        runner.verify(candidate, inputs.digest(manifest))
        self.file.write_bytes(b'changed')
        with self.assertRaisesRegex(runner.Failed, 'inputs hash changed'):
            runner.verify(candidate, inputs.digest(manifest))

    def test_commands_use_only_supplied_executable_and_firmware_paths(self):
        paths = {key: '/explicit/' + key for key in inputs.PATH_KEYS}
        tpm, qemu = runner.commands(self.root, self.root / 'media', 'seal', paths)
        self.assertEqual(tpm[0], paths['swtpm'])
        self.assertEqual(qemu[0], paths['qemu'])
        self.assertIn('if=pflash,unit=0,format=raw,readonly=on,file=/explicit/firmware_code', qemu)


if __name__ == '__main__': unittest.main()

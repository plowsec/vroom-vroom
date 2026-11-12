import base64
import importlib.util
import io
import hashlib
import json
from pathlib import Path
import tarfile
import tempfile
import os
import shutil
import subprocess
import unittest


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).parents[1] / filename)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


bundle = load("bundle", "prepare-hv-smoke.py")
transfer = load("transfer", "hv-smoke-transfer.py")
RUN = "vroom-pilot-0123456789ab"


def archive_bytes(files):
    stream = io.BytesIO()
    with tarfile.open(fileobj=stream, mode="w:gz") as archive:
        for name, content, kind in files:
            info = tarfile.TarInfo(name)
            info.type = kind
            info.size = len(content)
            archive.addfile(info, io.BytesIO(content))
    return stream.getvalue()


class TransferTests(unittest.TestCase):
    def test_transfer_size_caps_remain_bounded(self):
        self.assertEqual(transfer.MAX_ARCHIVE_BYTES, 12 * 1024 * 1024)
        self.assertEqual(transfer.MAX_EXTRACTED_BYTES, 64 * 1024 * 1024)
        self.assertEqual(transfer.CHUNK_BYTES, 18 * 1024)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); path = root / 'oversized.tar.gz'
            with path.open('wb') as stream:
                stream.truncate(transfer.MAX_ARCHIVE_BYTES + 1)
            with self.assertRaises(transfer.TransferError):
                transfer.generate(path, RUN, 'i-0123456789abcdef0', root / 'requests')
            self.assertFalse((root / 'requests').exists())
            with self.assertRaises(transfer.TransferError):
                transfer.extract(RUN, '0' * 64, 1, transfer.MAX_ARCHIVE_BYTES + 1, root)

    def test_traversal_absolute_symlinks_and_duplicates_rejected(self):
        for members in ([('../outside', b'x', tarfile.REGTYPE)], [('/outside', b'x', tarfile.REGTYPE)],
                        [('link', b'', tarfile.SYMTYPE)], [('link', b'', tarfile.LNKTYPE)],
                        [('same', b'a', tarfile.REGTYPE), ('same', b'b', tarfile.REGTYPE)]):
            with self.subTest(members=members), tarfile.open(fileobj=io.BytesIO(archive_bytes(members)), mode="r:gz") as archive:
                with self.assertRaises(transfer.TransferError):
                    transfer.safe_members(archive)

    def test_wrong_run_or_corrupt_chunk_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'run-id').write_text('different-run')
            data = b'content'
            with self.assertRaisesRegex(transfer.TransferError, 'ownership'):
                transfer.receive(RUN, transfer.sha(data), 0, transfer.sha(data), base64.b64encode(data), root)
            (root / 'run-id').write_text(RUN)
            with self.assertRaisesRegex(transfer.TransferError, 'hash'):
                transfer.receive(RUN, transfer.sha(data), 0, '0' * 64, base64.b64encode(data), root)

    def test_reassembly_hash_and_every_extracted_file_verified(self):
        content = b'fn main() {}\n'
        inventory = json.dumps({'files': {'src/main.rs': {'size': len(content), 'sha256': transfer.sha(content)}}}).encode()
        data = archive_bytes([('src/main.rs', content, tarfile.REGTYPE), ('BUNDLE-MANIFEST.json', inventory, tarfile.REGTYPE)])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'run-id').write_text(RUN)
            checksum = transfer.sha(data)
            transfer.receive(RUN, checksum, 0, checksum, base64.b64encode(data), root)
            # Identical repeated chunks are idempotent, never appended.
            transfer.receive(RUN, checksum, 0, checksum, base64.b64encode(data), root)
            result = transfer.extract(RUN, checksum, 1, len(data), root)
            self.assertTrue(result['verified'])
            source = Path(result['source_path'])
            (source / 'src/main.rs').write_text('altered')
            with self.assertRaisesRegex(transfer.TransferError, 'hash differs'):
                transfer.extract(RUN, checksum, 1, len(data), root)

    def test_generated_ssm_requests_stay_below_limit_and_have_no_cloud_calls(self):
        content = b''.join(hashlib.sha256(str(index).encode()).digest() for index in range(1600))
        data = archive_bytes([('file', content, tarfile.REGTYPE)])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); archive = root / 'bundle.tar.gz'; archive.write_bytes(data)
            plan = transfer.generate(archive, RUN, 'i-0123456789abcdef0', root / 'requests')
            self.assertLessEqual(plan['maximum_request_bytes'], 32768)
            self.assertGreater(plan['chunk_count'], 1)
            self.assertEqual(plan['ordered_requests'][0], '0000-bootstrap.json')
            self.assertTrue(plan['ordered_requests'][-1].endswith('-extract.json'))
            for filename in plan['ordered_requests']:
                request = json.loads((root / 'requests' / filename).read_text())
                self.assertEqual(request['InstanceIds'], ['i-0123456789abcdef0'])


class BundleTests(unittest.TestCase):
    def test_smm_source_profile_has_exact_example_and_no_media_helpers(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary).resolve()
            names = ['crates/hv/build.rs', 'crates/linux-harness/examples/smm_flash.rs']
            for crate in bundle.LINUX_CRATES:
                names += ['crates/' + crate + '/Cargo.toml', 'crates/' + crate + '/src/lib.rs']
            for name in names:
                path = root / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_text('fixture')
            sources = {path.as_posix() for path in bundle.selected_sources(root, 'smm_flash')}
            self.assertEqual(sources, set(names))
            self.assertFalse(set(bundle.LINUX_SCRIPTS) & sources)
            self.assertEqual(bundle.workload_target('smm_flash'), 'x86_64-unknown-linux-gnu')
            (root / 'crates/linux-harness/examples/smm_flash.rs').unlink()
            with self.assertRaisesRegex(bundle.BundleError, 'source missing'):
                bundle.selected_sources(root, 'smm_flash')
        script = bundle.build_script('smm_flash')
        self.assertIn('exec timeout -k 5s 360s sh', script)
        self.assertIn('20s target/x86_64-unknown-linux-gnu/debug/examples/smm_flash', script)
        self.assertIn('CARGO_NET_OFFLINE=true', script)
        self.assertNotIn('qemu', script)
        self.assertNotIn('heartbeat', script)
        self.assertNotIn('select-packaged-kernel', script)

    def test_combined_arm_profile_includes_both_workloads_and_hard_outer_bound(self):
        sources = {path.as_posix() for path in bundle.selected_sources(bundle.ROOT, 'linux_harness_arm64')}
        self.assertIn('crates/hv/examples/smoke.rs', sources)
        self.assertIn('crates/hv/src/kvm/aarch64.rs', sources)
        self.assertIn('crates/linux-harness/src/linux_arm64.rs', sources)
        self.assertTrue(set(bundle.ARM_SCRIPTS) <= sources)
        self.assertNotIn('crates/hv/src/kvm/x86_64.rs', sources)
        script = bundle.build_script('linux_harness_arm64')
        self.assertIn('exec timeout -k 5s 420s sh', script)
        self.assertIn('--architecture arm64', script)
        self.assertIn('15s target/aarch64-unknown-linux-gnu/debug/examples/smoke', script)
        self.assertIn('70s target/aarch64-unknown-linux-gnu/debug/vroom-linux-harness', script)
        self.assertLess(script.index('check-arm-kernel.py'), script.index('cargo test'))
        self.assertIn('select-packaged-kernel.py --architecture arm64 --require-config', script)
        self.assertLess(script.index('select-packaged-kernel.py'), script.index('check-arm-kernel.py'))
        self.assertNotIn('/boot/vmlinuz', script)
        self.assertNotIn('qemu', script)

    def test_arm_smoke_has_linux_arm_backend_and_bounded_native_wrapper(self):
        sources = {path.as_posix() for path in bundle.selected_sources(bundle.ROOT, 'smoke')}
        self.assertIn('crates/hv/examples/smoke.rs', sources)
        self.assertIn('crates/hv/src/kvm/aarch64.rs', sources)
        self.assertIn('crates/hv/src/kvm/gic.rs', sources)
        self.assertNotIn('crates/hv/src/kvm/x86_64.rs', sources)
        self.assertFalse(any('/hvf/' in path or path.startswith('research/') for path in sources))
        self.assertEqual(bundle.workload_target('smoke'), 'aarch64-unknown-linux-gnu')
        script = bundle.build_script('smoke')
        self.assertIn('[ "$(uname -m)" = aarch64 ] || exit 2', script)
        self.assertIn('180s', script)
        self.assertIn('15s target/aarch64-unknown-linux-gnu/debug/examples/smoke', script)
        self.assertIn('CARGO_NET_OFFLINE=true', script)
        self.assertNotIn('x86_64-unknown-linux-gnu', script)
        with self.assertRaises(bundle.BundleError):
            bundle.workload_target('unknown')

    def test_reviewed_wrappers_and_backend_selection(self):
        # CPU smoke remains frozen; Linux wrapper adds the reviewed RPM provenance gate.
        for example, checksum in {
            'smoke_x86_64': 'aac458c1c844acf43cd05bc4f3d0ee33ca3ddcecea5bf09584d59848772ffb4a',
            'linux_harness': '61e8333f6dccccbf54ad736f6e9ec2927653669957b24125e074408d1e1e31f0',
        }.items():
            with self.subTest(example=example):
                self.assertEqual(bundle.digest(bundle.build_script(example).encode()), checksum)
                self.assertEqual(bundle.workload_target(example), 'x86_64-unknown-linux-gnu')
                sources = {path.as_posix() for path in bundle.selected_sources(bundle.ROOT, example)}
                self.assertIn('crates/hv/src/kvm/x86_64.rs', sources)
                self.assertNotIn('crates/hv/src/kvm/aarch64.rs', sources)
                self.assertNotIn('crates/hv/src/kvm/gic.rs', sources)

    def test_linux_harness_scope_includes_only_declared_sources_and_helpers(self):
        sources = {path.as_posix() for path in bundle.selected_sources(bundle.ROOT, 'linux_harness')}
        self.assertIn('crates/linux-harness/src/linux.rs', sources)
        self.assertIn('crates/arch/src/x86_64/loader.rs', sources)
        self.assertIn('crates/devices/src/uart16550.rs', sources)
        self.assertTrue(set(bundle.LINUX_SCRIPTS) <= sources)
        self.assertNotIn('crates/block/src/rollback.rs', sources)
        self.assertFalse(any('/hvf/' in path or path.startswith('research/') for path in sources))
        self.assertFalse(any('/examples/' in path for path in sources))
        script = bundle.build_script('linux_harness')
        self.assertIn('300s', script)
        self.assertIn('70s', script)
        self.assertIn('CARGO_NET_OFFLINE=true', script)
        self.assertIn('select-packaged-kernel.py --architecture x86_64', script)
        self.assertLess(script.index('select-packaged-kernel.py'), script.index('cargo test'))
        self.assertNotIn('/boot/vmlinuz', script)
        self.assertNotIn('qemu', script)

    def test_failed_kernel_proof_stops_shell_before_build_or_guest(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            tools = root / 'bin'; tools.mkdir()
            scripts = root / 'scripts'; scripts.mkdir()
            # The gate runs an actual isolated Python process with a rejecting selector.
            (scripts / 'select-packaged-kernel.py').write_text('raise SystemExit(2)\n')
            (root / 'build-and-run.sh').write_text(bundle.build_script('linux_harness'))
            for name, content in {
                'uname': '#!/bin/sh\ncase "$1" in -s) echo Linux;; -m) echo x86_64;; esac\n',
                'rustc': '#!/bin/sh\nexit 0\n',
                'cargo': '#!/bin/sh\n[ "$1" = --version ] && exit 0\ntouch attempted-build\n',
                'timeout': '#!/bin/sh\nshift 3\nexec "$@"\n',
            }.items():
                path = tools / name; path.write_text(content); path.chmod(0o755)
            (tools / 'python3').symlink_to(shutil.which('python3'))
            result = subprocess.run(['/bin/sh', str(root / 'build-and-run.sh')],
                env={**os.environ, 'PATH': str(tools) + ':/usr/bin:/bin'}, capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 2, result.stderr)
            self.assertFalse((root / 'attempted-build').exists())
            self.assertFalse((root / 'heartbeat.cpio').exists())

    def test_linux_harness_extra_crates_are_not_allowed_in_original_smoke(self):
        lock = {'package': [{'name': 'vroom-linux-harness', 'version': '0.1.0'}]}
        bundle.check_lock(lock, lock, 'linux_harness')
        with self.assertRaisesRegex(bundle.BundleError, 'unrelated'):
            bundle.check_lock(lock, lock, 'smoke_x86_64')
        unrelated = {'package': [{'name': 'vroom-vmm', 'version': '0.1.0'}]}
        with self.assertRaisesRegex(bundle.BundleError, 'unrelated'):
            bundle.check_lock(unrelated, unrelated, 'linux_harness')

    def test_changed_staged_source_is_rejected_even_when_root_is_unchanged(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); stage = root / 'stage'; stage.mkdir()
            relative = 'crates/hv/src/lib.rs'
            for base in (root, stage):
                path = base / relative; path.parent.mkdir(parents=True); path.write_text('reviewed')
            (stage / 'SOURCE-MANIFEST.json').write_text(json.dumps({'source_files': {relative: bundle.digest(b'reviewed')}}))
            (stage / relative).write_text('unexpected edit')
            with self.assertRaisesRegex(bundle.BundleError, 'staged source changed'):
                bundle.pack(stage, root / 'bad.tar.gz', root)

    def test_generated_build_input_drift_is_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); stage = root / 'stage'; stage.mkdir()
            for name in bundle.GENERATED_FILES:
                path = stage / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_text('reviewed')
            (stage / 'SOURCE-MANIFEST.json').write_text(json.dumps({'source_files': {},
                'generated_files': {name: bundle.digest(b'reviewed') for name in bundle.GENERATED_FILES}}))
            (stage / 'build-and-run.sh').write_text('unexpected command')
            with self.assertRaisesRegex(bundle.BundleError, 'generated build input changed'):
                bundle.pack(stage, root / 'bad.tar.gz', root)

    def test_source_change_after_staging_blocks_freeze(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); stage = root / 'stage'; stage.mkdir()
            source = root / 'crates/hv/src/lib.rs'; source.parent.mkdir(parents=True); source.write_text('new edit')
            (stage / 'SOURCE-MANIFEST.json').write_text(json.dumps({
                'source_files': {'crates/hv/src/lib.rs': bundle.digest(b'previous source')}}))
            with self.assertRaisesRegex(bundle.BundleError, 'changed since staging'):
                bundle.pack(stage, root / 'frozen.tar.gz', root)
            self.assertFalse((root / 'frozen.tar.gz').exists())

    def test_dependency_version_drift_is_rejected(self):
        original = {'package': [{'name': 'libc', 'version': '1', 'source': bundle.REGISTRY, 'checksum': 'a'}]}
        changed = {'package': [{'name': 'libc', 'version': '2', 'source': bundle.REGISTRY, 'checksum': 'b'}]}
        with self.assertRaisesRegex(bundle.BundleError, 'changed'):
            bundle.check_lock(original, changed)

    def test_unrelated_workspace_packages_are_rejected(self):
        lock = {'package': [{'name': 'unrelated-research', 'version': '1'}]}
        with self.assertRaisesRegex(bundle.BundleError, 'unrelated'):
            bundle.check_lock(lock, lock)

    def test_pack_is_deterministic_and_excludes_build_state(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); stage = root / 'stage'; stage.mkdir()
            for name in ('Cargo.toml', 'Cargo.lock', 'build-and-run.sh', '.cargo/config.toml', 'crates/hv/src/lib.rs', 'vendor/test/lib.rs'):
                path = stage / name; path.parent.mkdir(parents=True, exist_ok=True); path.write_text(name)
            (stage / 'SOURCE-MANIFEST.json').write_text(json.dumps({'source_files': {},
                'first_party_files': ['crates/hv/src/lib.rs'],
                'generated_files': {name: bundle.digest((stage / name).read_bytes()) for name in bundle.GENERATED_FILES}}))
            (stage / 'target').mkdir(); (stage / 'target/secret').write_text('do not include')
            (stage / 'crates/hv/secret.key').write_text('do not include')
            first = bundle.pack(stage, root / 'one.tar.gz', root)
            second = bundle.pack(stage, root / 'two.tar.gz', root)
            self.assertEqual(first['sha256'], second['sha256'])
            with tarfile.open(root / 'one.tar.gz') as archive:
                self.assertNotIn('target/secret', archive.getnames())
                self.assertNotIn('crates/hv/secret.key', archive.getnames())


if __name__ == '__main__':
    unittest.main()

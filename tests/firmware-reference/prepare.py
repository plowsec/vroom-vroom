#!/usr/bin/env python3
"""Build disposable, file-only UEFI reference media. Does NOT start a guest.

Host effects are restricted to a new private output directory and child compiler,
OpenSSL, signing and mtools processes. No mount, NVRAM, keychain, physical device,
existing VM, network or cloud command exists in this tool.
"""
from __future__ import annotations
import argparse, datetime, hashlib, json, os, pathlib, shutil, struct, subprocess, uuid
import inputs

BASE = pathlib.Path(__file__).resolve().parent
GLOBAL = uuid.UUID('8be4df61-93ca-11d2-aa0d-00e098032b8c').bytes_le
DB = uuid.UUID('d719b2cb-3d3a-4596-a3bc-dad00e67656f').bytes_le
X509 = uuid.UUID('a5c059a1-94e4-4aa7-87b5-ab155c2bf072').bytes_le
PKCS7 = uuid.UUID('4aafd29d-68df-49ee-8aa9-347d375665a7').bytes_le
ATTRS = 0x27

def digest(path):
    with open(path, 'rb') as f:
        return hashlib.file_digest(f, 'sha256').hexdigest()

def run(argv, tools, cwd=None, timeout=30):
    env = {**os.environ, 'LC_ALL': 'C', 'MTOOLS_SKIP_CHECK': '0'}
    if pathlib.Path(argv[0]) == tools['lld_link']:
        env['DYLD_LIBRARY_PATH'] = str(tools['llvm_library'].parent)
    result = subprocess.run([str(x) for x in argv], cwd=cwd, stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=timeout,
                            env=env)
    if result.returncode:
        # Keys and unseal data are never command arguments or diagnostic output.
        raise RuntimeError(f'{pathlib.Path(argv[0]).name} failed ({result.returncode}): ' +
                           result.stdout[-4096:].decode('utf-8', 'replace'))
    return result.stdout

def efi_time(now):
    return struct.pack('<HBBBBBBIhBB', now.year, now.month, now.day, now.hour, now.minute, now.second, 0, 0, 0, 0, 0)

def esl(cert_der, owner):
    size = 16 + len(cert_der)
    return X509 + struct.pack('<III', 28 + size, 0, size) + owner + cert_der

def auth2(name, vendor, body, signer, keys, work, timestamp, tools):
    payload = name.encode('utf-16le') + vendor + struct.pack('<I', ATTRS) + timestamp + body
    label = name + '-' + signer + '-' + timestamp.hex()
    data, sig = work / (label + '.signed-data'), work / (label + '.p7')
    data.write_bytes(payload)
    run([tools['openssl'], 'cms', '-sign', '-binary', '-noattr', '-md', 'sha256', '-outform', 'DER',
         '-in', data, '-signer', keys / (signer + '.crt'), '-inkey', keys / (signer + '.key'), '-out', sig], tools)
    run([tools['openssl'], 'cms', '-verify', '-binary', '-inform', 'DER', '-in', sig, '-content', data,
         '-noverify', '-out', os.devnull], tools)
    signature = sig.read_bytes()
    return timestamp + struct.pack('<IHH', 24 + len(signature), 0x200, 0xef1) + PKCS7 + signature + body

def c_array(name, data):
    return 'static const U8 ' + name + '[] = {' + ','.join(f'0x{x:02x}' for x in data) + '};\n'

def compile_efi(source, dest, generated, tools):
    obj = dest.with_suffix('.obj')
    run([tools['clang'], '--target=x86_64-pc-windows-msvc', '-ffreestanding', '-fshort-wchar',
         '-fno-stack-protector', '-mno-red-zone', '-mno-stack-arg-probe', '-fno-builtin',
         '-O1', '-g0', '-nostdlib', '-Werror', '-I', BASE / 'src', '-I', generated,
         '-c', source, '-o', obj], tools)
    run([tools['lld_link'], '/subsystem:efi_application', '/entry:efi_main', '/nodefaultlib', '/machine:x64',
         '/dynamicbase', '/nxcompat', '/timestamp:0', '/out:' + str(dest), obj], tools)
    obj.unlink()
    data = dest.read_bytes()
    pe = struct.unpack_from('<I', data, 0x3c)[0]
    if data[pe:pe+4] != b'PE\0\0' or struct.unpack_from('<H', data, pe+4)[0] != 0x8664:
        raise ValueError('not an x64 PE image')
    if struct.unpack_from('<H', data, pe+24+68)[0] != 10:
        raise ValueError('not an EFI application')

def sign_pe(source, dest, keys, tools):
    run([tools['osslsigncode'], 'sign', '-certs', keys / 'DB.crt', '-key', keys / 'DB.key', '-h', 'sha256',
         '-in', source, '-out', dest], tools)
    run([tools['osslsigncode'], 'verify', '-CAfile', keys / 'DB.crt', '-ignore-cdp', '-ignore-crl', '-in', dest], tools)

def cpio(pid1, token):
    out = bytearray()
    entries = [('dev', 0o040755, b'', 0, 0), ('dev/console', 0o020600, b'', 5, 1),
               ('dev/null', 0o020666, b'', 1, 3), ('proc', 0o040755, b'', 0, 0),
               ('sys', 0o040755, b'', 0, 0), ('etc', 0o040755, b'', 0, 0),
               ('init', 0o100755, pid1, 0, 0),
               ('etc/vroom-reference-token', 0o100400, token.encode(), 0, 0)]
    for ino, (name, mode, data, major, minor) in enumerate(entries + [('TRAILER!!!', 0, b'', 0, 0)], 1):
        nameb = name.encode() + b'\0'
        fields = [ino, mode, 0, 0, 1, 0, len(data), 0, 0, major, minor, len(nameb), 0]
        out.extend(b'070701' + ''.join(f'{x:08x}' for x in fields).encode() + nameb)
        out.extend(b'\0' * (-len(out) % 4)); out.extend(data); out.extend(b'\0' * (-len(out) % 4))
    out.extend(b'\0' * (-len(out) % 512))
    if len(out) > 16 * 1024 * 1024:
        raise ValueError('initrd exceeds bound')
    return bytes(out)

def static_x64(path):
    data = path.read_bytes()
    if data[:6] != b'\x7fELF\x02\x01' or struct.unpack_from('<H', data, 18)[0] != 62:
        raise ValueError('PID1 must be x64 ELF64 little endian')
    off = struct.unpack_from('<Q', data, 32)[0]
    entsize, count = struct.unpack_from('<HH', data, 54)
    if entsize != 56 or not (1 <= count <= 64) or off + count * entsize > len(data):
        raise ValueError('ELF program headers')
    for i in range(count):
        kind, flags, fileoff, _, _, filesz, _, _ = struct.unpack_from('<IIQQQQQQ', data, off + i * entsize)
        if kind == 3 or (kind == 1 and flags & 3 == 3):
            raise ValueError('ELF dynamic interpreter or writable executable segment')
        if kind == 2:
            if fileoff + filesz > len(data) or filesz % 16:
                raise ValueError('ELF dynamic bounds')
            for j in range(fileoff, fileoff + filesz, 16):
                tag = struct.unpack_from('<Q', data, j)[0]
                if tag == 1:
                    raise ValueError('ELF DT_NEEDED')
                if tag == 0:
                    break

def fat_image(path, files, tools):
    # This is a new regular file, never a host block device or mounted volume.
    with path.open('xb') as f:
        f.truncate(64 * 1024 * 1024)
    run([tools['mformat'], '-i', path, '-F', '-v', 'VROOMREF', '::'], tools)
    run([tools['mmd'], '-i', path, '::/EFI', '::/EFI/BOOT'], tools)
    for name, source in files.items():
        run([tools['mcopy'], '-i', path, source, '::/EFI/BOOT/' + name], tools)
    path.chmod(0o400)

def prepare(out, pid_manifest_path, tools, kernel_hash):
    out = inputs.output_path(out)
    if out.exists() or out.is_symlink():
        raise ValueError('output must be new')
    tools = inputs.validate_paths({key: str(value) for key, value in tools.items()})
    if digest(tools['kernel']) != kernel_hash:
        raise ValueError('expected kernel input hash mismatch')
    pid_manifest_path = inputs.output_path(pid_manifest_path)
    pid_manifest = json.loads(pid_manifest_path.read_text())
    if (pid_manifest.get('schema') != 1 or
            pid_manifest.get('scope') != 'local_qemu_firmware_tpm_reference_pid1' or
            pid_manifest.get('source_sha256') != inputs.pid1_sources()):
        raise ValueError('PID1 build source provenance mismatch')
    if (pid_manifest.get('build_script_sha256') != digest(BASE / 'build-pid1.py') or
            pid_manifest.get('inputs_script_sha256') != digest(BASE / 'inputs.py') or
            pid_manifest.get('prepare_script_sha256') != digest(BASE / 'prepare.py')):
        raise ValueError('PID1 build wrapper provenance mismatch')
    pid1 = inputs.output_path(pid_manifest['binary']['path'])
    if digest(pid1) != pid_manifest['binary']['sha256']:
        raise ValueError('PID1 binary provenance mismatch')
    pid_sources = [BASE / 'pid1' / name for name in pid_manifest['source_sha256']]
    static_x64(pid1)
    os.umask(0o077); out.mkdir(mode=0o700)
    keys = out / 'private-keys'; keys.mkdir(mode=0o700)
    work = out / 'generated'; work.mkdir(mode=0o700)
    token = uuid.uuid4().hex
    try:
        (out / 'INITRD.CPIO').write_bytes(cpio(pid1.read_bytes(), token))
        hashes, esls, auths = {}, {}, {}
        owner = uuid.uuid4().bytes_le
        now = datetime.datetime.now(datetime.timezone.utc)
        timestamp = efi_time(now)
        for key in ('PK', 'KEK', 'DB', 'UNTRUSTED'):
            run([tools['openssl'], 'req', '-new', '-x509', '-newkey', 'rsa:2048', '-sha256', '-nodes', '-days', '7',
                 '-subj', '/CN=Disposable Vroom Reference ' + key, '-addext', 'basicConstraints=critical,CA:TRUE',
                 '-addext', 'keyUsage=critical,digitalSignature,keyCertSign', '-addext', 'extendedKeyUsage=codeSigning',
                 '-keyout', keys / (key + '.key'), '-out', keys / (key + '.crt')], tools)
            der = run([tools['openssl'], 'x509', '-in', keys / (key + '.crt'), '-outform', 'DER'], tools)
            hashes[key] = hashlib.sha256(der).hexdigest(); esls[key] = esl(der, owner)
            (out / (key + '.der')).write_bytes(der)
        for key, name, vendor, signer in [('DB', 'db', DB, 'KEK'), ('KEK', 'KEK', GLOBAL, 'PK'), ('PK', 'PK', GLOBAL, 'PK')]:
            auths[key] = auth2(name, vendor, esls[key], signer, keys, work, timestamp, tools)
        header = ''.join(c_array(k + '_ESL', esls[k]) + c_array(k + '_AUTH', auths[k]) +
                         '#define ' + k + '_FINGERPRINT "' + hashes[k] + '"\n' for k in ('PK', 'KEK', 'DB'))
        for label, signer, delta in [('DB_UPDATE_1', 'KEK', 1), ('DB_UPDATE_2', 'KEK', 2), ('DB_UNAUTHORIZED', 'UNTRUSTED', 3)]:
            update = auth2('db', DB, esls['DB'], signer, keys, work, efi_time(now + datetime.timedelta(seconds=delta)), tools)
            header += c_array(label, update)
        header += '#define UNTRUSTED_FINGERPRINT "' + hashes['UNTRUSTED'] + '"\n'
        compile_efi(BASE / 'src/probe.c', out / 'UNSIGNED.EFI', work, tools)
        unsigned_hash = digest(out / 'UNSIGNED.EFI')
        header += c_array('UNSIGNED_SHA256', bytes.fromhex(unsigned_hash))
        header += '#define UNSIGNED_HASH_HEX "' + unsigned_hash + '"\n#define UNSIGNED_SIZE ' + str((out / 'UNSIGNED.EFI').stat().st_size) + 'ULL\n'
        initrd_hash = digest(out / 'INITRD.CPIO')
        header += c_array('INITRD_SHA256', bytes.fromhex(initrd_hash))
        header += '#define INITRD_HASH_HEX "' + initrd_hash + '"\n#define INITRD_SIZE ' + str((out / 'INITRD.CPIO').stat().st_size) + 'ULL\n'
        (work / 'fixture_public.h').write_text(header)
        compile_efi(BASE / 'src/reference.c', out / 'ENROLL.EFI', work, tools)
        sign_pe(out / 'ENROLL.EFI', out / 'BOOTX64.EFI', keys, tools)
        sign_pe(out / 'UNSIGNED.EFI', out / 'SIGNED.EFI', keys, tools)
        sign_pe(tools['kernel'], out / 'LINUX.EFI', keys, tools)
        common = {n: out / n for n in ('SIGNED.EFI', 'UNSIGNED.EFI', 'LINUX.EFI', 'INITRD.CPIO')}
        fat_image(out / 'enroll.fat', {**common, 'BOOTX64.EFI': out / 'ENROLL.EFI'}, tools)
        fat_image(out / 'accept.fat', {**common, 'BOOTX64.EFI': out / 'BOOTX64.EFI'}, tools)
    finally:
        # Private signing keys are unnecessary once the images are signed.
        shutil.rmtree(keys)
    input_paths = [*tools.values(), pid1, pid_manifest_path]
    source_paths = [BASE / 'prepare.py', BASE / 'run.py', BASE / 'inputs.py', BASE / 'build-pid1.py',
                    *sorted((BASE / 'src').glob('*')), *pid_sources,
                    *sorted((BASE / 'tests').glob('*.py'))]
    manifest = {'schema': 1, 'scope': 'local_qemu_firmware_tpm_reference_only', 'token': token,
                'private_signing_keys_deleted': not keys.exists(), 'certificate_sha256': hashes,
                'kernel_original_sha256': kernel_hash,
                'paths': {key: str(path) for key, path in tools.items()}, 'pid1_sha256': digest(pid1),
                'initrd_sha256': initrd_hash, 'boot_count': 3, 'per_boot_seconds': 180,
                'unsigned_probe_sha256': unsigned_hash,
                'campaign_seconds': 600, 'cpus': 1, 'ram_mib': 1024,
                'inputs': {str(p.resolve()): digest(p) for p in input_paths},
                'sources': {str(p.resolve()): digest(p) for p in source_paths},
                'files': {str(p.relative_to(out)): digest(p) for p in sorted(out.rglob('*')) if p.is_file()},
                'created_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                'guest_execution_authorized': False}
    (out / 'manifest.json').write_text(json.dumps(manifest, indent=2, sort_keys=True) + '\n')
    return {'output': str(out), 'manifest_sha256': digest(out / 'manifest.json'), 'pid1_sha256': digest(pid1),
            'private_signing_keys_deleted': True, 'guest_started': False}

if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=pathlib.Path, required=True)
    parser.add_argument('--pid1-build', type=pathlib.Path, required=True)
    parser.add_argument('--inputs', type=pathlib.Path, required=True)
    args = parser.parse_args()
    tools, kernel_hash = inputs.load(args.inputs)
    print(json.dumps(prepare(args.output, args.pid1_build, tools, kernel_hash), sort_keys=True))

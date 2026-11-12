#!/usr/bin/env python3
"""At most three local reference boots. Default: read-only command preview.
--execute-reviewed checks artifact drift; root guest-execution GO is also needed.
"""
from __future__ import annotations
import argparse, datetime, hashlib, json, os, pathlib, resource, shutil, signal, socket, stat, subprocess, tempfile, time
import inputs
BASE = pathlib.Path(__file__).resolve().parent
LIMIT = 2 * 1024 * 1024
BOOT_WORK_SECONDS = 160
CAMPAIGN_WORK_SECONDS = 570
CLEANUP_WAIT_BOUND_SECONDS = 16  # two processes * (TERM2+KILL2) * two cleanup passes
SIGNALS = {signal.SIGINT, signal.SIGTERM, signal.SIGALRM}
class Failed(RuntimeError): pass
class Unreaped(Failed): pass
def require(condition, message):
    if not condition: raise Failed(message)
def digest(path):
    with open(path, 'rb') as f: return hashlib.file_digest(f, 'sha256').hexdigest()
def verify(candidate, expected):
    candidate = inputs.output_path(candidate)
    manifest_path = candidate / 'manifest.json'
    require(not candidate.is_symlink() and candidate.is_dir(), 'candidate directory')
    require(digest(manifest_path) == expected, 'reviewed manifest changed')
    m = json.loads(manifest_path.read_text())
    require(m['scope'] == 'local_qemu_firmware_tpm_reference_only', 'scope')
    paths = inputs.validate_paths(m.get('paths'))
    require(all(str(path) in m['inputs'] for path in paths.values()), 'tool and image input inventory')
    require((m['boot_count'], m['per_boot_seconds'], m['campaign_seconds'], m['cpus'], m['ram_mib']) == (3, 180, 600, 1, 1024), 'bounds')
    require(m['private_signing_keys_deleted'] is True and not (candidate / 'private-keys').exists(), 'private signing keys')
    require(len(m['token']) == 32 and all(c in '0123456789abcdef' for c in m['token']), 'token')
    inventory = {str(p.relative_to(candidate)) for p in candidate.rglob('*') if p.is_file() and p != manifest_path}
    require(inventory == set(m['files']), 'artifact inventory changed')
    for rel, sha in m['files'].items():
        p = candidate / rel
        require(not pathlib.PurePosixPath(rel).is_absolute() and '..' not in pathlib.PurePosixPath(rel).parts, 'artifact path')
        require(not p.is_symlink() and p.is_file() and digest(p) == sha, 'artifact hash changed')
    for group in ('inputs', 'sources'):
        for name, sha in m[group].items(): require(digest(name) == sha, group + ' hash changed')
    require(not any(p.is_symlink() for p in candidate.rglob('*')), 'artifact symlink')
    return m
def commands(private, media, phase, paths):
    # Writable paths are inside new owned private state; no host directory share.
    tpm = [str(paths['swtpm']), 'socket', '--tpm2', '--tpmstate', f'dir={private}/tpm,mode=0600,lock',
           '--ctrl', f'type=unixio,path={private}/tpm.sock,mode=0600,terminate',
           '--log', f'file={private}/{phase}-tpm.log,level=1']
    qemu = [str(paths['qemu']), '-no-user-config', '-nodefaults', '-machine', 'pc-q35-11.1,smm=on,mem-merge=off',
            '-accel', 'tcg,thread=single', '-cpu', 'max', '-smp', '1', '-m', '1024',
            '-global', 'driver=cfi.pflash01,property=secure,value=on',
            '-drive', f"if=pflash,unit=0,format=raw,readonly=on,file={paths['firmware_code']}",
            '-drive', f'if=pflash,unit=1,format=raw,file={private}/vars.fd',
            '-drive', f'if=none,id=esp,format=raw,readonly=on,file={media}',
            '-device', 'virtio-blk-pci,drive=esp,bootindex=1',
            '-chardev', f'socket,id=chrtpm,path={private}/tpm.sock',
            '-tpmdev', 'emulator,id=tpm0,chardev=chrtpm', '-device', 'tpm-tis,tpmdev=tpm0',
            '-chardev', f'file,id=serial,path={private}/{phase}-serial.log',
            '-device', 'isa-serial,chardev=serial', '-qmp', f'unix:{private}/qmp.sock,server=on,wait=off',
            '-monitor', 'none', '-display', 'none', '-nic', 'none', '-no-reboot',
            '-boot', 'order=c,strict=on', '-name', 'disposable-vroom-reference']
    return tpm, qemu
def child_limits():
    signal.pthread_sigmask(signal.SIG_UNBLOCK, SIGNALS)
    resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    resource.setrlimit(resource.RLIMIT_FSIZE, (LIMIT, LIMIT))
class Owner:
    def __init__(self): self.children = []
    def spawn(self, argv, stdout):
        old = signal.pthread_sigmask(signal.SIG_BLOCK, SIGNALS)
        try:
            p = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=stdout, stderr=subprocess.STDOUT,
                                 start_new_session=True, preexec_fn=child_limits,
                                 env={'PATH': '/usr/bin:/bin', 'LC_ALL': 'C'})
            self.children.append(p)
            return p
        finally: signal.pthread_sigmask(signal.SIG_SETMASK, old)
    def stop(self, p):
        if p.poll() is None:
            try: os.killpg(p.pid, signal.SIGTERM)
            except ProcessLookupError: pass
            try: p.wait(timeout=2)
            except subprocess.TimeoutExpired:
                try: os.killpg(p.pid, signal.SIGKILL)
                except ProcessLookupError: pass
                try: p.wait(timeout=2)
                except subprocess.TimeoutExpired as exc: raise Unreaped('owned child unreaped') from exc
        p.wait(timeout=0)
    def stop_all(self):
        failures = []
        for p in reversed(self.children):
            try: self.stop(p)
            except BaseException as exc: failures.append(exc)
        if failures: raise Unreaped('owned process cleanup incomplete') from failures[0]
def wait_path(path, process, deadline):
    while not path.exists():
        require(process.poll() is None, 'process exited before socket')
        require(time.monotonic() < deadline, 'socket readiness deadline')
        time.sleep(0.05)
    require(stat.S_ISSOCK(path.stat().st_mode), 'expected private Unix socket')
def drain_qmp(sock, buffer, events):
    while True:
        try: chunk = sock.recv(65536)
        except BlockingIOError: break
        if not chunk: break
        buffer.extend(chunk); require(len(buffer) <= 131072, 'QMP output bound')
        while b'\n' in buffer:
            line, _, rest = buffer.partition(b'\n'); buffer[:] = rest
            if not line.strip(): continue
            obj = json.loads(line)
            if 'event' in obj:
                require(len(events) < 64, 'QMP event bound'); events.append(obj)
def one_boot(owner, private, candidate, phase, campaign_deadline, paths):
    # one_boot.finally and run.finally can each attempt both children. Reserve
    # all16s of waits rather than assuming the first cleanup will succeed.
    deadline = min(time.monotonic() + BOOT_WORK_SECONDS, campaign_deadline - 20)
    require(deadline - time.monotonic() >= 150, 'insufficient campaign time for next boot')
    for name in ('tpm.sock', 'qmp.sock'):
        path = private / name
        if path.exists():
            require(stat.S_ISSOCK(path.stat().st_mode), 'stale socket type'); path.unlink()
    media = candidate / ('enroll.fat' if phase == 'enroll' else 'accept.fat')
    tpm_cmd, qemu_cmd = commands(private, media, phase, paths)
    sock = None; tpm = qemu = None; events = []; buffer = bytearray(); start = time.monotonic()
    try:
        with (private / (phase + '-tpm-process.log')).open('xb') as log: tpm = owner.spawn(tpm_cmd, log)
        wait_path(private / 'tpm.sock', tpm, min(deadline, time.monotonic() + 10))
        require((private / 'tpm.sock').stat().st_mode & 0o777 == 0o600, 'TPM socket permissions')
        with (private / (phase + '-qemu.log')).open('xb') as log: qemu = owner.spawn(qemu_cmd, log)
        wait_path(private / 'qmp.sock', qemu, min(deadline, time.monotonic() + 10))
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM); sock.settimeout(3)
        sock.connect(str(private / 'qmp.sock')); sock.sendall(b'{"execute":"qmp_capabilities"}\n'); sock.setblocking(False)
        while qemu.poll() is None:
            require(time.monotonic() < deadline, 'per-boot watchdog expired')
            require(tpm.poll() is None, 'TPM exited during guest')
            for p in private.glob('*.log'): require(p.stat().st_size <= LIMIT, 'log size bound')
            drain_qmp(sock, buffer, events); time.sleep(0.05)
        drain_qmp(sock, buffer, events)
        require(qemu.returncode == 0, 'QEMU exit status')
        require(any(e.get('event') == 'SHUTDOWN' and e.get('data', {}).get('guest') is True and
                    e.get('data', {}).get('reason') == 'guest-shutdown' for e in events), 'guest shutdown event absent')
        serial = (private / (phase + '-serial.log')).read_bytes(); require(len(serial) <= LIMIT, 'serial bound')
        return {'phase': phase, 'elapsed_seconds': round(time.monotonic() - start, 3),
                'qemu_exit_code': qemu.returncode, 'guest_shutdown_observed': True,
                'serial_sha256': hashlib.sha256(serial).hexdigest(), 'serial_bytes': len(serial),
                'serial': serial.decode('utf-8', 'replace')}
    finally:
        issue = None
        for p in (qemu, tpm):
            if p is not None:
                try: owner.stop(p)
                except BaseException as exc: issue = exc
        try:
            if sock is not None: sock.close()
        finally:
            if issue is not None: raise Unreaped('boot cleanup incomplete') from issue

def records(serial, prefix):
    found = []
    for line in serial.replace('\r', '').splitlines():
        pos = line.find(prefix)
        if pos >= 0:
            payload = line[pos + len(prefix):]; require(len(payload) <= 8192, 'record size')
            found.append(json.loads(payload))
    return found
def failure_evidence(private):
    evidence = []
    for phase in ('enroll', 'seal', 'unseal'):
        entry = {'phase': phase}
        path = private / (phase + '-serial.log')
        if path.is_file() and path.stat().st_size <= LIMIT:
            raw = path.read_bytes(); entry['serial_sha256'] = hashlib.sha256(raw).hexdigest()
            entry['serial_bytes'] = len(raw); entry['diagnostic_records'] = []
            for prefix in ('VROOM_EFI_REFERENCE ', 'VROOM_TPM_REFERENCE '):
                try: rows = records(raw.decode('utf-8', 'replace'), prefix)
                except (ValueError, Failed): rows = []
                for row in rows[:4]:
                    safe = {}
                    for key in ('status', 'phase', 'stage', 'efi_status_hex', 'tpm_rc'):
                        v = row.get(key)
                        if isinstance(v, str) and len(v) <= 80 and all(c.isalnum() or c in '_-' for c in v): safe[key] = v
                        elif type(v) is int and 0 <= v < 2**32: safe[key] = v
                    entry['diagnostic_records'].append(safe)
        path = private / (phase + '-qemu.log')
        if path.is_file() and path.stat().st_size <= LIMIT:
            raw = path.read_bytes(); entry['qemu_log_sha256'] = hashlib.sha256(raw).hexdigest()
            # QEMU default diagnostics, not guest serial or swtpm packet logging.
            entry['qemu_diagnostic_tail'] = raw[-4096:].decode('utf-8', 'replace')
        if len(entry) > 1: evidence.append(entry)
    return evidence
def accept(result, manifest, previous):
    phase, serial = result['phase'], result.pop('serial')
    efis = records(serial, 'VROOM_EFI_REFERENCE ')
    require(len(efis) == 1 and efis[0].get('status') == 'pass', 'EFI acceptance')
    efi = efis[0]
    for name in ('PK', 'KEK', 'DB'):
        require(efi.get(name.lower() + '_sha256') == manifest['certificate_sha256'][name], 'enrolled certificate fingerprint')
    if phase == 'enroll':
        require(efi.get('phase') == 'enroll' and efi.get('initial_setup_mode') == 1 and
                efi.get('initial_secure_boot') == 0 and efi.get('setup_mode_after') == 0, 'enrollment flags')
        require('VROOM_SIGNED_PROBE_EXECUTED' not in serial and not records(serial, 'VROOM_TPM_REFERENCE '), 'unexpected enrollment workload')
        result['efi'] = efi; return
    require(efi.get('phase') == 'acceptance' and efi.get('secure_boot') == 1 and efi.get('setup_mode') == 0, 'secure boot flags')
    for field in ('signed_child_executed', 'unsigned_child_refused', 'initrd_hash_verified',
                  'unauthorized_variable_update_rejected', 'authorized_variable_update_accepted', 'db_contents_unchanged',
                  'unsigned_image_not_started', 'unsigned_probe_hash_verified'):
        require(efi.get(field) is True, field)
    status = efi.get('unsigned_refusal_status_hex')
    require(status in ('800000000000000f', '800000000000001a'), 'unsigned documented refusal status')
    require(efi.get('unsigned_image_handle_loaded') is (status == '800000000000001a'), 'unsigned refusal handle semantics')
    require(efi.get('unsigned_probe_sha256') == manifest['unsigned_probe_sha256'], 'unsigned known payload hash')
    require(efi.get('untrusted_signer_sha256') == manifest['certificate_sha256']['UNTRUSTED'], 'untrusted certificate fingerprint')
    require(efi.get('authorized_update_step') == (1 if phase == 'seal' else 2), 'authorized variable update transition')
    require(efi.get('initrd_sha256') == manifest['initrd_sha256'], 'signed initrd digest')
    require(serial.count('VROOM_SIGNED_PROBE_EXECUTED') == 1, 'signed probe execution count')
    tpms = records(serial, 'VROOM_TPM_REFERENCE '); require(len(tpms) == 1, 'TPM record count'); tpm = tpms[0]
    require(tpm.get('schema') == 1 and tpm.get('token') == manifest['token'] and
            tpm.get('phase') == phase and tpm.get('status') == 'pass', 'TPM result identity')
    for key in ('replay_matches', 'secure_boot_measured', 'password_auth_rejected', 'empty_policy_rejected', 'policy_unseal_verified'):
        require(tpm.get(key) is True, 'TPM ' + key)
    require(type(tpm.get('pcr7_event_count')) is int and 1 <= tpm['pcr7_event_count'] <= 4096, 'PCR7 event count')
    for key in ('pcr7_sha256', 'event_log_sha256', 'sealed_blob_sha256', 'secret_hash'):
        value = tpm.get(key, '')
        require(isinstance(value, str) and len(value) == 64 and all(c in '0123456789abcdef' for c in value), 'TPM hash syntax')
    if previous is not None:
        for key in ('pcr7_sha256', 'sealed_blob_sha256', 'secret_hash'):
            require(tpm[key] == previous['tpm'][key], 'cold boot continuity ' + key)
    result['efi'], result['tpm'] = efi, tpm
def run(candidate, expected, report):
    candidate, report = inputs.output_path(candidate), inputs.output_path(report)
    m = verify(candidate, expected); require(not report.exists(), 'report must be new')
    paths = inputs.validate_paths(m['paths'])
    marker = candidate.parent / (candidate.name + '.campaign-started')
    with marker.open('x') as f: f.write(datetime.datetime.now(datetime.timezone.utc).isoformat() + '\n')
    os.umask(0o077)
    private = pathlib.Path(tempfile.mkdtemp(prefix='vr-fw-', dir='/tmp'))
    owner = Owner(); retained = False; outcomes = []; error = None; diagnostics = []
    started = time.monotonic(); old_handlers = {s: signal.getsignal(s) for s in SIGNALS}
    def interrupted(signum, frame): raise Failed('campaign interrupted or watchdog expired')
    for s in SIGNALS: signal.signal(s, interrupted)
    # Leave30s for cleanup; both cleanup passes fit within the600s campaign bound.
    signal.setitimer(signal.ITIMER_REAL, CAMPAIGN_WORK_SECONDS)
    try:
        shutil.copyfile(paths['firmware_vars'], private / 'vars.fd'); (private / 'vars.fd').chmod(0o600)
        (private / 'tpm').mkdir(mode=0o700)
        for phase in ('enroll', 'seal', 'unseal'):
            require(digest(candidate / 'accept.fat') == m['files']['accept.fat'], 'acceptance media drift')
            outcome = one_boot(owner, private, candidate, phase, started + CAMPAIGN_WORK_SECONDS, paths)
            accept(outcome, m, outcomes[-1] if phase == 'unseal' else None)
            outcomes.append(outcome)
    except BaseException as exc:
        error = type(exc).__name__ + ': ' + str(exc)[:1024]
    finally:
        old_mask = signal.pthread_sigmask(signal.SIG_BLOCK, SIGNALS)
        signal.setitimer(signal.ITIMER_REAL, 0)
        try:
            try: owner.stop_all()
            except BaseException as exc:
                retained = True; error = type(exc).__name__ + ': ' + str(exc)[:1024]
            if error is not None:
                try: diagnostics = failure_evidence(private)
                except Exception: diagnostics = [{'evidence_collection_failed': True}]
            if not retained: shutil.rmtree(private)
        finally:
            for s, handler in old_handlers.items(): signal.signal(s, handler)
            signal.pthread_sigmask(signal.SIG_SETMASK, old_mask)
    result = {'schema': 1, 'scope': m['scope'], 'status': 'pass' if error is None and len(outcomes) == 3 else 'fail',
              'manifest_sha256': expected, 'boots': outcomes, 'elapsed_seconds': round(time.monotonic() - started, 3),
              'all_owned_processes_reaped': not retained, 'disposable_state_removed': not private.exists(),
              'retained_state_path': str(private) if retained else None, 'error': error,
              'failure_evidence': diagnostics,
              'claims': {'qemu_reference_only': True, 'native_vroom': False, 'windows': False,
                         'bitlocker': False, 'variable_storage_attack_resistance': False}}
    report.write_text(json.dumps(result, indent=2, sort_keys=True) + '\n'); report.chmod(0o600)
    print(json.dumps({k: result[k] for k in ('status', 'elapsed_seconds', 'all_owned_processes_reaped', 'disposable_state_removed', 'error')}))
    return 0 if result['status'] == 'pass' else 1
if __name__ == '__main__':
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--candidate', type=pathlib.Path, required=True)
    p.add_argument('--execute-reviewed', metavar='MANIFEST_SHA256')
    p.add_argument('--report', type=pathlib.Path)
    a = p.parse_args()
    if a.execute_reviewed:
        if not a.report: p.error('--report required for execution')
        raise SystemExit(run(a.candidate.resolve(), a.execute_reviewed, a.report.resolve()))
    candidate = inputs.output_path(a.candidate)
    manifest = verify(candidate, digest(candidate / 'manifest.json'))
    print(json.dumps({phase: commands(pathlib.Path('/tmp/OWNED-PRIVATE'), candidate / ('enroll.fat' if phase == 'enroll' else 'accept.fat'), phase, manifest['paths']) for phase in ('enroll', 'seal', 'unseal')}, indent=2))

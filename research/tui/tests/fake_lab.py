"""A stand-in vroom-lab agent socket: the protocol and result shapes of
research/kernel/src/agent.rs and main.rs, over a toy two-CPU machine."""
import json
import os
import socketserver
import struct
import tempfile
import threading
import time

KERNEL = 0xfffff80010000000
DRIVER = 0xfffff80020000000
USER_DLL = 0x7ffa00000000
PROCESS = {'eprocess': 0xffffa00000001000, 'pid': 4242, 'parent_pid': 4, 'name': 'app.exe',
           'directory_table': 0x1234000, 'peb': 0x7ff000}


def _param(name, kind='integer', required=False):
    return {'name': name, 'type': kind, 'required': required, 'help': name}


def _row(name, params=(), risk='read', job_only=False):
    return {'name': name, 'category': name.split('.')[0], 'summary': name, 'params': list(params),
            'mutates': risk != 'read', 'risk': risk, 'job_only': job_only}


CPU = _param('cpu')
ADDRESS = _param('address', 'address', True)
COMMANDS = [
    _row('vm.status'), _row('vm.pause', risk='control'), _row('vm.resume', [CPU], 'control'),
    _row('vm.step', [CPU], 'control'), _row('vm.step_over', [CPU, _param('vector_length')], 'control'),
    _row('vm.step_out', [CPU, _param('vector_length')], 'control'),
    _row('vm.snapshot', [_param('name', 'string'), _param('path', 'path'), _param('description', 'string')], 'write'),
    _row('vm.snapshots'),
    _row('vm.restore', [_param('name', 'string'), _param('path', 'path'), _param('resume', 'boolean')], 'destructive'),
    _row('vm.shutdown', risk='destructive'),
    _row('cpu.registers', [CPU]),
    _row('cpu.write_register', [CPU, _param('register', 'string', True), _param('value', 'address', True)], 'write'),
    _row('memory.read_virtual', [CPU, ADDRESS, _param('length'), _param('context', 'boolean'),
                                 _param('skip_unmapped', 'boolean')]),
    _row('memory.write_virtual', [CPU, ADDRESS, _param('hex', 'string', True), _param('context', 'boolean')], 'write'),
    _row('debug.breakpoints', [_param('addresses', 'array', True)], 'control'),
    _row('debug.watchpoints', [_param('watches', 'array', True)], 'control'),
    _row('debug.traces', [_param('limit'), _param('clear', 'boolean'), _param('memory', 'object')]),
    _row('debug.faults', [_param('enable', 'boolean'), _param('limit'), _param('clear', 'boolean')], 'control'),
    _row('kernel.info', [CPU]),
    _row('kernel.symbols', [CPU, _param('base', 'address'), _param('context', 'boolean')]),
    _row('kernel.processes', [CPU]),
    _row('kernel.process', [CPU, _param('pid')]),
    _row('kernel.peb', [CPU, _param('pid', required=True)]),
    _row('kernel.user_modules', [CPU, _param('pid', required=True)]),
    _row('kernel.modules', [CPU]),
    _row('kernel.callbacks', [CPU]),
    _row('kernel.stack', [CPU, _param('vector_length')]),
    _row('kernel.export', [CPU, _param('base', 'address', True), _param('path', 'path'),
                           _param('allow_missing', 'boolean')], 'write'),
    _row('kernel.process_event', [CPU]),
    _row('fuzz.run', [_param('harness', 'path', True)], 'control', job_only=True),
]
INSPECTION = ('cpu.', 'memory.', 'kernel.', 'debug.breakpoints', 'debug.watchpoints', 'vm.step', 'vm.resume')


def pe_image(base):
    """A flat ARM64 PE with one DIR64 relocation, as kernel.export writes."""
    data = bytearray(0x3000)
    data[:2] = b'MZ'
    struct.pack_into('<I', data, 0x3c, 0x80)
    data[0x80:0x84] = b'PE\0\0'
    struct.pack_into('<H', data, 0x84, 0xaa64)
    struct.pack_into('<Q', data, 0xb0, base)
    struct.pack_into('<IIHH', data, 0x2000, 0x1000, 12, 0xa000, 0xa008)
    struct.pack_into('<QQ', data, 0x1000, base + 0x1234, 0xffff800000008000)
    return bytes(data)


class Lab:
    def __init__(self, exports):
        self.lock = threading.RLock()
        self.more = threading.Condition(self.lock)
        self.exports = exports
        self.cpus = 2
        self.pc = [KERNEL + 0x1000, KERNEL + 0x2000]
        self.sp = [0xffff800000010000, 0xffff800000020000]
        self.x = [[cpu * 100 + i for i in range(31)] for cpu in range(2)]
        self.memory = {}
        self.state = 'Paused'
        self.breakpoints = []
        self.watches = []
        self.process = None
        self.symbols = False
        self.events = []
        self.seq = 0
        self.calls = []
        self.snapshots = [{'name': 'snap-1', 'path': '/snapshots/snap-1', 'created_unix': 1, 'description': 'vroom-lab',
                           'bytes': 3 << 20}]
        self.runner = None

    def event(self, payload):
        with self.lock:
            self.seq += 1
            self.events.append({**payload, 'seq': self.seq})
            self.more.notify_all()

    def poll(self, after, wait_ms):
        deadline = time.monotonic() + min(wait_ms, 30000) / 1000
        with self.lock:
            after = self.seq if after is None else after
            while self.seq <= after:
                left = deadline - time.monotonic()
                if left <= 0:
                    break
                self.more.wait(left)
            rows = [e for e in self.events if e['seq'] > after][:100]
            return {'events': rows, 'next': rows[-1]['seq'] if rows else after, 'latest': self.seq}

    def registers(self, cpu):
        return {'cpu': cpu, 'x': self.x[cpu], 'pc': self.pc[cpu], 'sp': self.sp[cpu], 'pstate': 0x60000005,
                'ttbr0': 0x1234000 if cpu == 0 else 0x5678000, 'ttbr1': 0x9000, 'tcr': 0x1, 'sctlr': 0x2, 'vbar': 0x3,
                'sve_vl': None}

    def _run_until_breakpoint(self, cpu, stop):
        for _ in range(2000):
            time.sleep(0.001)
            with self.lock:
                if stop.is_set():
                    return
                self.pc[cpu] += 4
                if self.pc[cpu] in self.breakpoints:
                    self.state = 'Paused'
                    self.event({'event': 'stopped', 'cpu': cpu, 'pc': self.pc[cpu], 'reason': 'Breakpoint', 'syndrome': 0})
                    return

    def run(self, name, args):
        self.calls.append((name, dict(args)))
        cpu = int(args.get('cpu', 0))
        with self.lock:
            if name.startswith(INSPECTION) and self.state != 'Paused':
                op = name.split('.')[1]
                raise RuntimeError(f'the VM is running: vm.pause before {op}, and vm.resume when done')
            if name == 'vm.status':
                return {'state': self.state, 'cpus': self.cpus, 'breakpoints': self.breakpoints}
            if name == 'vm.pause':
                if self.runner is not None:
                    self.runner.set()
                self.state = 'Paused'
                return {'state': 'Paused'}
            if name == 'vm.resume':
                self.state = 'Running'
                self.runner = threading.Event()
                threading.Thread(target=self._run_until_breakpoint, args=(cpu, self.runner), daemon=True).start()
                return 'running'
            if name in ('vm.step', 'vm.step_over', 'vm.step_out'):
                self.pc[cpu] += 4
                reason = {'vm.step': 'SingleStep', 'vm.step_over': 'StepOver', 'vm.step_out': 'StepOut'}[name]
                self.event({'event': 'stopped', 'cpu': cpu, 'pc': self.pc[cpu], 'reason': reason, 'syndrome': 0})
                return 'stepping' if name == 'vm.step' else {'state': 'running', 'until': self.pc[cpu]}
            if name == 'vm.snapshots':
                return list(self.snapshots)
            if name == 'vm.snapshot':
                snapshot = {'name': args.get('name') or f'snap-{len(self.snapshots) + 1}', 'description': 'vroom-lab',
                            'bytes': 1 << 20}
                self.snapshots.append(snapshot)
                return {'name': snapshot['name'], 'path': f"/snapshots/{snapshot['name']}"}
            if name == 'vm.restore':
                self.breakpoints, self.watches, self.process, self.symbols = [], [], None, False
                self.pc = [KERNEL + 0x1000, KERNEL + 0x2000]
                return {'restored': f"/snapshots/{args['name']}", 'state': 'Paused'}
            if name == 'vm.shutdown':
                self.state = 'Stopped'
                self.event({'event': 'shutdown'})
                return 'stopped'
            if name == 'cpu.registers':
                return self.registers(cpu)
            if name == 'cpu.write_register':
                register, value = args['register'], int(str(args['value']), 0) if isinstance(args['value'], str) else args['value']
                if register == 'pc':
                    self.pc[cpu] = value
                elif register.startswith('x'):
                    self.x[cpu][int(register[1:])] = value
                else:
                    raise RuntimeError(f'unknown register {register}')
                return self.registers(cpu)
            if name == 'memory.read_virtual':
                length = int(args.get('length', 256))
                if length > 65536:
                    raise RuntimeError('read limit is 64 KiB')
                address = args['address']
                data = bytes(self.memory.get(address + i, (address + i) & 0xff) for i in range(length))
                return {'address': address, 'hex': data.hex()}
            if name == 'memory.write_virtual':
                data = bytes.fromhex(args['hex'])
                if len(data) > 16384:
                    raise RuntimeError('write requires 1..16384 hex-encoded bytes')
                for i, byte in enumerate(data):
                    self.memory[args['address'] + i] = byte
                return {'address': args['address'], 'written': len(data)}
            if name == 'debug.breakpoints':
                if len(args['addresses']) > 4:
                    raise RuntimeError('at most four 4-byte-aligned addresses')
                if any(a >> 63 == 0 for a in args['addresses']) and self.process is None:
                    raise RuntimeError('select a process before setting a user-address breakpoint')
                self.breakpoints = list(args['addresses'])
                return self.breakpoints
            if name == 'debug.watchpoints':
                self.watches = list(args['watches'])
                return self.watches
            if name == 'debug.traces':
                return {'records': [{'seq': 1, 'kind': 'tracepoint', 'tracepoint': 0, 'hit': 1, 'cpu': 0,
                                     'pc': KERNEL + 0x3000, 'sp': 0, 'pstate': 0, 'ttbr0': 0, 'x': [0] * 31,
                                     'actions_applied': 0, 'cntvct': 1, 'time_ns': 2}],
                        'remaining': 1, 'recorded': 1, 'dropped': 0, 'capacity': 4096, 'tracepoints': [], 'hwatch': []}
            if name == 'debug.faults':
                return {'catching': None, 'records': [{
                    'seq': 7, 'cpu': 1, 'summary': 'access violation: read of 0x8 at app.exe+0x10 (0x7ff600001010), '
                    'app.exe pid 4242, user mode, first chance', 'code': 0xc0000005, 'pc': 0x7ff600001010,
                    'mode': 'user', 'first_chance': True, 'stack': ['app.exe+0x10']}], 'remaining': 1}
            if name == 'kernel.info':
                return {'base': KERNEL, 'size': 0x3000, 'name': 'ntoskrnl.exe', 'pdb': None, 'export_count': 1}
            if name == 'kernel.symbols':
                base = args.get('base', KERNEL)
                if base == DRIVER:
                    raise RuntimeError('no PDB for this module')
                self.symbols = True
                path = os.path.join(self.exports, f'symbols-{base:016x}.pdb')
                with open(path, 'wb') as stream:
                    stream.write(b'pdb')
                return {'count': 1234, 'base': base, 'path': path}
            if not self.symbols and name in ('kernel.modules', 'kernel.callbacks', 'kernel.processes', 'kernel.process'):
                raise RuntimeError('load matching kernel symbols first')
            if name == 'kernel.modules':
                return [{'base': KERNEL, 'size': 0x3000, 'name': 'ntoskrnl.exe', 'path': '\\SystemRoot\\system32\\ntoskrnl.exe'},
                        {'base': DRIVER, 'size': 0x3000, 'name': 'driver.sys', 'path': '\\SystemRoot\\driver.sys'}]
            if name == 'kernel.processes':
                return [{'eprocess': 0xffffa00000000100, 'pid': 4, 'parent_pid': 0, 'name': 'System',
                         'directory_table': 0x1aa000, 'peb': 0}, PROCESS]
            if name == 'kernel.process':
                if 'pid' not in args:
                    self.process = None
                    return {'context': 'cpu'}
                if args['pid'] != PROCESS['pid']:
                    raise RuntimeError('PID not found')
                self.process = PROCESS
                return PROCESS
            if name in ('kernel.peb', 'kernel.user_modules'):
                if self.process is None:
                    raise RuntimeError('select a process first')
                if name == 'kernel.peb':
                    return {'address': 0x7ff000, 'image_base': 0x7ff600000000, 'ldr': 0x7ff100, 'parameters': 0x7ff200,
                            'image_path': 'C:\\app.exe', 'command_line': 'app.exe', 'errors': []}
                return [{'base': USER_DLL, 'size': 0x3000, 'name': 'ntdll.dll', 'path': 'C:\\Windows\\System32\\ntdll.dll'}]
            if name == 'kernel.callbacks':
                return [{'slot': 0, 'block': 0xffffa00000002000, 'function': self.pc[0], 'context': None,
                         'layout_source': 'pdb', 'module': 'driver.sys', 'module_offset': 0x40},
                        {'slot': 3, 'block': 0xffffa00000003000, 'function': DRIVER + 0x80, 'context': None,
                         'layout_source': 'pdb', 'module': None, 'module_offset': None}]
            if name == 'kernel.stack':
                return {'frames': [{'pc': self.pc[cpu], 'sp': self.sp[cpu], 'module': 'ntoskrnl.exe',
                                    'symbol': 'nt!KiSomething+0x10', 'method': 'xdata'},
                                   {'pc': DRIVER + 0x44, 'sp': self.sp[cpu] + 0x40, 'module': 'driver.sys',
                                    'symbol': 'driver.sys+0x44', 'method': 'frame'}], 'stop': 'end of stack'}
            if name == 'kernel.process_event':
                return {'kind': 'create', 'process': PROCESS['eprocess'], 'pid': PROCESS['pid'], 'parent_pid': 4,
                        'creator_pid': 4, 'creator_tid': 8, 'image': '\\??\\C:\\app.exe', 'command_line': 'app.exe',
                        'creation_status': 0, 'exact_image_name': True, 'layout_source': 'pdb'}
            if name == 'kernel.export':
                base = args['base']
                path = os.path.join(self.exports, f'image-{base:016x}-{len(self.calls)}.exe')
                with open(path, 'xb') as stream:
                    stream.write(pe_image(base))
                return {'path': path, 'image': {'base': base, 'size': 0x3000, 'name': 'image', 'timestamp': 0,
                                                'pdb': None, 'relocation_directory': [0x2000, 12]},
                        'missing_page_rvas': [], 'source': {'kind': 'live', 'runtime_base': base}}
        raise KeyError(name)


class _Handler(socketserver.StreamRequestHandler):
    def handle(self):
        server, actor = self.server, None
        server.clients.append(self.connection)
        while line := self.rfile.readline():
            request = json.loads(line)
            method, params = request['method'], request.get('params') or {}
            try:
                if method == 'initialize':
                    actor = params['actor']
                    result = {'actor': actor, 'server': 'vroom-lab'}
                elif actor is None:
                    raise PermissionError('call initialize with an actor first')
                else:
                    result = server.dispatch(method, params)
                reply = {'jsonrpc': '2.0', 'id': request['id'], 'result': result}
            except KeyError as error:
                reply = {'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32601, 'message': f'unknown {error}'}}
            except Exception as error:  # noqa: BLE001
                reply = {'jsonrpc': '2.0', 'id': request['id'], 'error': {'code': -32603, 'message': str(error)}}
            try:
                self.wfile.write(json.dumps(reply).encode() + b'\n')
                self.wfile.flush()
            except OSError:
                return


class LabServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True

    def __init__(self):
        self.directory = tempfile.mkdtemp(prefix='vlab', dir='/tmp')
        self.path = os.path.join(self.directory, 'lab.sock')
        self.lab = Lab(self.directory)
        self.clients = []
        super().__init__(self.path, _Handler)
        threading.Thread(target=self.serve_forever, daemon=True).start()

    def dispatch(self, method, params):
        lab = self.lab
        if method == 'commands.list':
            return COMMANDS
        if method == 'events.poll':
            return lab.poll(params.get('after'), int(params.get('wait_ms', 0)))
        if method == 'commands.run':
            value = lab.run(params['command'], params.get('args') or {})
            if isinstance(value, list) and len(value) > 200:
                raise AssertionError('no paging in this stand-in')
            return value
        if any(row['name'] == method for row in COMMANDS):
            # A command is also a method.
            return lab.run(method, params)
        raise KeyError(method)

    def close(self):
        if self.lab.runner is not None:
            self.lab.runner.set()
        self.shutdown()
        self.server_close()
        for name in os.listdir(self.directory):
            os.unlink(os.path.join(self.directory, name))
        os.rmdir(self.directory)

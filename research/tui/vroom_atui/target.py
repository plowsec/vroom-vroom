"""vroom-lab as an ATUI execution target, over its agent socket.

The lab speaks ATUI's agent protocol, so this is a binding (which lab
command implements each contract operation) plus what the contract has no
words for: the selected CPU, the selected process (user addresses and user
modules go through its address space), kernel symbols, and module images
exported for static analysis. Every lab command is also proxied as
`vroom.<command>`.

Stops: vm.step, vm.step_over, vm.step_out and vm.resume answer at once and
the stop arrives as a `stopped` event; vm.pause answers once paused and
reports no event.
"""
from functools import partial

from atui.services.commands import CommandError, CommandSpec
from atui.target import ModuleInfo
from atui.target.jsonrpc import JsonRpcTarget, RemoteBinding, RemoteOp, RemoteStop

from .analysis import prepare_rva_image
from .coverage import compare_artifacts, load_artifacts
from .lab import Lab, ensure

READ_CHUNK = 65536
WRITE_CHUNK = 16384
SYSTEM_REGISTERS = ('ttbr0', 'ttbr1', 'tcr', 'sctlr', 'vbar')
# ATUI's name for a register → the lab's.
LAB_REGISTER = {'flags': 'pstate'}
STOP_REASON = {
    'Breakpoint': 'breakpoint', 'SoftwareBreakpoint': 'breakpoint',
    'Watchpoint': 'watchpoint', 'Hwatch': 'watchpoint',
    'SingleStep': 'step', 'StepOver': 'step', 'StepOut': 'run',
}


def registers(answer) -> dict[str, int]:
    """cpu.registers → ATUI's AARCH64 names; PSTATE's NZCV is `flags`."""
    regs = {f'x{i}': value for i, value in enumerate(answer['x'])}
    regs.update(sp=answer['sp'], pc=answer['pc'], flags=answer['pstate'])
    regs.update((name, answer[name]) for name in SYSTEM_REGISTERS if name in answer)
    return regs


class VroomTarget(JsonRpcTarget):
    def __init__(self, path, *, lab: Lab | None = None, name='vroom'):
        self.cpu = 0
        #: The lab's selected process (kernel.process row), or None: the CPU's.
        self.process = None
        #: Stopped on exit when this target started it.
        self.lab = lab
        #: Bumped by every restore: a restored lab is a new boot.
        self.boot = 0
        self.coverage = None
        self._symbols = None
        self._module_cache = None
        self._images = {}
        super().__init__(str(path), self._binding(), name=name, label=f'vroom {path}', actor='atui', timeout=300)

    def _binding(self) -> RemoteBinding:
        cpu = lambda: {'cpu': self.cpu}
        return RemoteBinding(
            label='vroom-lab',
            arch='AARCH64',
            status=RemoteOp('vm.status', result=lambda answer: {'running': answer.get('state') == 'Running'}),
            registers=RemoteOp('cpu.registers', args=cpu, result=registers),
            read_memory=RemoteOp('memory.read_virtual',
                                 args=lambda address, size: {'cpu': self.cpu, 'address': address, 'length': size}),
            read_chunk=READ_CHUNK,
            write_memory=RemoteOp('memory.write_virtual',
                                  args=lambda address, data: {'cpu': self.cpu, 'address': address, 'hex': data.hex()}),
            write_chunk=WRITE_CHUNK,
            write_register=RemoteOp('cpu.write_register', args=lambda name, value: {
                'cpu': self.cpu, 'register': LAB_REGISTER.get(name, name), 'value': value}),
            step=RemoteOp('vm.step', args=cpu),
            step_over=RemoteOp('vm.step_over', args=cpu),
            step_out=RemoteOp('vm.step_out', args=cpu),
            resume=RemoteOp('vm.resume', args=cpu),
            pause=RemoteOp('vm.pause', stops='call'),
            breakpoints=RemoteOp('debug.breakpoints', args=lambda addresses: {'addresses': list(addresses)}),
            hw_breakpoint_slots=4,
            watchpoints=RemoteOp('debug.watchpoints', args=lambda rows: {'watches': [
                {'address': row['address'], 'size': row['size'], 'mode': row['kind']} for row in rows]}),
            watchpoint_slots=2,
            snapshots=RemoteOp('vm.snapshots'),
            save_snapshot=RemoteOp('vm.snapshot', args=lambda name: {'name': name} if name else {}),
            restore_snapshot=RemoteOp('vm.restore', args=lambda name: {'name': name}),
            event=self._event,
        )

    def _event(self, event) -> RemoteStop | None:
        kind = event.get('event')
        if kind == 'stopped':
            if isinstance(event.get('cpu'), int):
                self.cpu = event['cpu']
            reason = str(event.get('reason') or '')
            message = f"{reason}: {event['fault']}" if event.get('fault') else reason
            return RemoteStop(STOP_REASON.get(reason, 'run'), event.get('pc'), message)
        if kind == 'fault':
            return RemoteStop('error', message=str(event.get('message') or 'fault'))
        if kind == 'reset_requested':
            return RemoteStop('pause', message=str(event.get('message') or kind))
        if kind == 'shutdown':
            return RemoteStop('finished', message='shutdown', gone=True)
        return None

    # -- inspection ------------------------------------------------------

    def inspect(self, command, **args):
        """A lab command that needs a paused VM."""
        self._require_paused()
        return self.run_remote(command, args)

    def symbols(self) -> dict:
        """Kernel and its verified PDB, loaded once per boot."""
        if self._symbols is None or self._symbols[0] != self.boot:
            kernel = self.inspect('kernel.info', cpu=self.cpu)
            loaded = self.inspect('kernel.symbols', cpu=self.cpu)
            self._symbols = (self.boot, {**kernel, 'symbols': loaded.get('count')})
        return self._symbols[1]

    def modules(self) -> tuple[ModuleInfo, ...]:
        """Kernel modules, plus the selected process's DLLs."""
        key = (self.generation, self.boot, (self.process or {}).get('pid'))
        if self._module_cache is not None and self._module_cache[0] == key:
            return self._module_cache[1]
        self.symbols()
        rows = list(self.inspect('kernel.modules', cpu=self.cpu))
        if self.process is not None:
            rows += self.inspect('kernel.user_modules', cpu=self.cpu, pid=self.process['pid'])
        modules = tuple(ModuleInfo(row['name'], row['base'], row['size'],
                                   image=partial(self.analysis_image, row['base'])) for row in rows)
        self._module_cache = (key, modules)
        return modules

    def analysis_image(self, base: int):
        """`(path, 0)`: the module exported from memory as an RVA copy."""
        key = (self.boot, (self.process or {}).get('pid'), base)
        if key not in self._images:
            exported = self.inspect('kernel.export', cpu=self.cpu, base=base, allow_missing=True)
            path, _ = prepare_rva_image(exported, allow_unrelocated=True)
            self._images[key] = (path, 0)
        return self._images[key]

    def select_cpu(self, cpu: int) -> dict:
        count = self.run_remote('vm.status').get('cpus', 1)
        if not 0 <= cpu < count:
            raise CommandError(f'CPU {cpu}: the VM has {count}', code='invalid_arguments')
        self.cpu = cpu
        self.refresh_now()
        return {'cpu': cpu}

    def select_process(self, pid: int | None = None) -> dict:
        """The address space user addresses go through; None: the CPU's."""
        self.symbols()
        answer = self.inspect('kernel.process', cpu=self.cpu, **({'pid': pid} if pid is not None else {}))
        self.process = answer if pid is not None else None
        self.refresh_now()
        return answer

    def refresh_now(self):
        """Read the state again and repaint everything showing this target."""
        if not self.running:
            self._refresh_state()
        self.refresh()

    # -- lifetime --------------------------------------------------------

    def restore_snapshot(self, name):
        self.boot += 1
        self.process = None
        return super().restore_snapshot(name)

    def detach(self):
        if self.lab is not None and self.lab.pid is None and self.connected:
            # Its shutdown event is this detach's, not a lost lab.
            self._closing.set()
            try:
                self.run_remote('vm.shutdown')
            except CommandError:
                pass
        super().detach()
        if self.lab is not None:
            self.lab.stop()

    # -- commands --------------------------------------------------------

    def commands(self) -> tuple[CommandSpec, ...]:
        prefix = self.name
        own = (
            CommandSpec(f'{prefix}.cpu', 'Select the CPU', lambda ctx, cpu: self.select_cpu(cpu),
                        category='vroom', args={'cpu': {'type': 'integer', 'minimum': 0}}, required=('cpu',),
                        description='Registers, stepping and memory follow this vCPU'),
            CommandSpec(f'{prefix}.process', 'Select a process', lambda ctx, pid=None: self.select_process(pid),
                        category='vroom', args={'pid': {'type': 'integer', 'minimum': 0}},
                        description="User addresses and modules through this process; no pid: the CPU's"),
            CommandSpec(f'{prefix}.discover', 'Kernel and symbols', lambda ctx: self.symbols(),
                        category='vroom', description='Find the kernel and load its verified PDB'),
            CommandSpec(f'{prefix}.coverage', 'Load fuzzer coverage', self._load_coverage,
                        category='vroom', args={'path': {'type': 'string'}, 'baseline': {'type': 'string'}},
                        required=('path',), description='vroom-fuzz coverage.json; baseline: compare two runs'),
            CommandSpec(f'{prefix}.coverage_blocks', 'Coverage blocks', self._coverage_blocks,
                        category='vroom', args={'function': {'type': 'string'}}, required=('function',),
                        description='Blocks of one function (module!function) with hit counts'),
        )
        return super().commands() + own

    def _load_coverage(self, ctx, path, baseline=None):
        summary, functions, blocks = (compare_artifacts(baseline, path) if baseline
                                      else load_artifacts(path))
        self.coverage = {'summary': summary, 'functions': functions, 'blocks': blocks, 'comparison': bool(baseline)}
        self.refresh()
        return summary

    def _coverage_blocks(self, ctx, function):
        if self.coverage is None:
            raise CommandError('no coverage loaded: vroom.coverage PATH', code='no_coverage')
        module, _, name = function.rpartition('!')
        rows = []
        for block in self.coverage['blocks']:
            if (block.get('function') or '<unknown>') == name and (not module or (block.get('module') or '<unknown>') == module):
                row = {'address': block['va'] if block.get('va') is not None else block['gpa'],
                       'hits': block['hits'], 'size': block['size']}
                if self.coverage['comparison']:
                    row.update(baseline_hits=block['baseline_hits'], change=block['coverage_change'])
                rows.append(row)
        return sorted(rows, key=lambda row: row['address'])


def parse_spec(spec: str) -> tuple[str, dict]:
    """`PATH[,restore=SNAPSHOT][,lab=BINARY][,keep|,stop]` → (path, options)."""
    path, *words = str(spec or '').split(',')
    options = {}
    for word in words:
        key, sep, value = word.partition('=')
        if key in ('keep', 'stop') and not sep:
            options[key] = True
        elif key in ('restore', 'lab') and sep:
            options[key] = value
        else:
            raise ValueError(f'unknown option {word!r}: restore=SNAPSHOT, lab=BINARY, keep, stop')
    if not path:
        raise ValueError('vroom needs an agent socket or a lab config: vroom:LAB.toml or vroom:lab.sock')
    return path, options


def open_vroom(spec: str, session=None) -> VroomTarget:
    """The `vroom` provider.

    `vroom:SOCKET` attaches to a running lab and leaves it running (`stop`:
    shut it down on detach). `vroom:LAB.toml` attaches to that config's lab,
    starting it if needed (`restore=SNAPSHOT` boots or restores from one);
    a lab started here stops on detach unless `keep`.
    """
    from pathlib import Path

    path, options = parse_spec(spec)
    path = Path(path).expanduser()
    if path.suffix == '.toml':
        lab = ensure(path, options.get('restore'), options.get('lab'))
        if owned:
            lab.own()
        owned = (lab.pid is not None and not options.get('keep')) or options.get('stop')
        socket_path = lab.socket
    else:
        if options.get('restore') or options.get('lab'):
            raise ValueError('restore= and lab= need a lab config, not a socket')
        # No pid: stopping it is vm.shutdown over the socket.
        lab, owned, socket_path = Lab(path), options.get('stop'), path
    try:
        return VroomTarget(socket_path, lab=lab if owned else None)
    except Exception:
        if owned:
            lab.stop()
        raise

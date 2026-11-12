"""Side panes over a vroom target: modules, processes, stack, callbacks..."""
from textual.binding import Binding

from atui.ui.target_panes import TargetTable


def hex64(value):
    return f'{value:016x}' if isinstance(value, int) else '—'


class ModulesPane(TargetTable):
    columns = ('Module', 'Base', 'Size')

    def rows(self):
        for module in self.target.modules():
            yield (module.name, hex64(module.base), f'{module.size // 1024}K'), module.base


class ProcessesPane(TargetTable):
    """Enter selects the process whose address space user addresses use."""

    columns = ('', 'PID', 'Image', 'Parent', 'EPROCESS')

    def rows(self):
        self.target.symbols()
        selected = (self.target.process or {}).get('pid')
        self._pids = [None]
        yield ('*' if selected is None else '', '—', 'CPU context', '', ''), None
        for process in self.target.inspect('kernel.processes', cpu=self.target.cpu):
            self._pids.append(process['pid'])
            yield ('*' if process['pid'] == selected else '', process['pid'], process['name'],
                   process['parent_pid'], hex64(process['eprocess'])), None

    def activate(self, index):
        pids = getattr(self, '_pids', [])
        if 0 <= index < len(pids):
            self.run_worker(lambda: self._select(pids[index]), thread=True, exclusive=True, group='vroom-process')

    def _select(self, pid):
        try:
            self.target.select_process(pid)
        except Exception as error:  # noqa: BLE001 - shown in the pane
            self.app.call_from_thread(self.failed, error)


class StackPane(TargetTable):
    columns = ('#', 'PC', 'SP', 'Symbol', 'Unwind')

    def load(self):
        stack = self.target.inspect('kernel.stack', cpu=self.target.cpu)
        rows = [((i, hex64(frame['pc']), hex64(frame['sp']), frame.get('symbol') or '', frame['method']), frame['pc'])
                for i, frame in enumerate(stack['frames'])]
        return rows, stack.get('stop')

    def show(self, data):
        rows, stop = data
        super().show(rows)
        if stop:
            self.show_message(stop)


class CallbacksPane(TargetTable):
    """Process-notify callbacks; `>` marks the one the CPU is stopped at."""

    columns = ('', 'Slot', 'Owner', 'Function')

    def rows(self):
        self.target.symbols()
        pc = (self.target.registers() or {}).get('pc')
        for callback in self.target.inspect('kernel.callbacks', cpu=self.target.cpu):
            owner = callback['module'] or 'unresolved'
            if callback['module_offset'] is not None:
                owner += f"+0x{callback['module_offset']:x}"
            yield ('>' if callback['function'] == pc else '', callback['slot'], owner,
                   hex64(callback['function'])), callback['function']


class EventPane(TargetTable):
    """A process create/exit callback's arguments, at its entry."""

    columns = ('Field', 'Value')

    def rows(self):
        event = self.target.inspect('kernel.process_event', cpu=self.target.cpu)
        yield ('Event', event['kind']), None
        yield ('PID', event['pid']), None
        yield ('EPROCESS', hex64(event['process'])), event['process']
        if event['kind'] == 'create':
            for label, key in (('Parent PID', 'parent_pid'), ('Creator PID', 'creator_pid'),
                               ('Creator TID', 'creator_tid'), ('Image', 'image'), ('Command line', 'command_line')):
                yield (label, event[key] if event[key] is not None else '—'), None
            status = event['creation_status']
            yield ('CreationStatus', f'0x{status:08x}' if status is not None else '—'), None


class PebPane(TargetTable):
    columns = ('Field', 'Value')

    def rows(self):
        process = self.target.process
        if process is None:
            raise RuntimeError('select a process first')
        peb = self.target.inspect('kernel.peb', cpu=self.target.cpu, pid=process['pid'])
        for key, value in peb.items():
            if isinstance(value, int):
                yield (key, hex(value)), value or None
            elif isinstance(value, list):
                yield (key, '; '.join(map(str, value)) or '—'), None
            else:
                yield (key, value or '—'), None


class FaultsPane(TargetTable):
    """debug.faults records, newest first; also while the VM runs."""

    columns = ('#', 'CPU', 'Mode', 'Summary')

    def rows(self):
        report = self.target.run_remote('debug.faults', {})
        for record in reversed(report.get('records') or ()):
            yield (record['seq'], record['cpu'], record['mode'], record['summary']), record['pc']


class TracesPane(TargetTable):
    """Tracepoint and hwatch records, newest first; also while the VM runs."""

    columns = ('#', 'Kind', 'CPU', 'PC', 'Address', 'Bytes')

    def rows(self):
        report = self.target.run_remote('debug.traces', {})
        for record in reversed(report.get('records') or ()):
            yield (record['seq'], record['kind'], record['cpu'], hex64(record['pc']),
                   hex64(record.get('address', record.get('pointer'))), (record.get('hex') or '')[:32]), record['pc']


class SnapshotsPane(TargetTable):
    """r restores the selected snapshot; s saves a new one."""

    BINDINGS = [Binding('r', 'restore', 'Restore'), Binding('s', 'save', 'Save')]
    columns = ('Name', 'Description', 'Size')

    def rows(self):
        self._names = []
        for snapshot in self.target.snapshots():
            self._names.append(snapshot['name'])
            yield (snapshot['name'], snapshot.get('description') or '', f"{(snapshot.get('bytes') or 0) >> 20}M"), None

    def activate(self, index):
        pass

    def action_restore(self):
        index = self.cursor_row
        names = getattr(self, '_names', [])
        if index is not None and index < len(names):
            self._run(lambda: self.target.restore_snapshot(names[index]))

    def action_save(self):
        self._run(lambda: self.target.save_snapshot(''))

    def _run(self, operation):
        def work():
            try:
                operation()
            except Exception as error:  # noqa: BLE001 - shown in the pane
                self.app.call_from_thread(self.failed, error)
                return
            self.app.call_from_thread(self.reload)
        self.run_worker(work, thread=True, exclusive=True, group='vroom-snapshot')


class CoveragePane(TargetTable):
    """vroom.coverage's functions; Enter goes to the function's first block."""

    columns = ('Function', 'Blocks', 'Coverage', 'Hits')

    def rows(self):
        coverage = self.target.coverage
        if coverage is None:
            raise RuntimeError('vroom.coverage PATH [baseline=PATH]')
        first = {}
        for block in coverage['blocks']:
            key = (block.get('module') or '<unknown>', block.get('function') or '<unknown>')
            address = block['va'] if block.get('va') is not None else block['gpa']
            first[key] = min(first.get(key, address), address)
        for row in coverage['functions']:
            if coverage['comparison']:
                blocks = f'{row.baseline_covered}/{row.total} → {row.covered}/{row.total}'
                hits = f'{row.baseline_hits:,} → {row.hits:,}'
            else:
                blocks, hits = f'{row.covered}/{row.total}', f'{row.hits:,}'
            yield (f'{row.module}!{row.name}', blocks, f'{row.percent:5.1f}%', hits), first.get((row.module, row.name))

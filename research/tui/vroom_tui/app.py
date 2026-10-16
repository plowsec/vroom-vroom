"""Windows kernel workspace: live state, modules and callback ownership."""
import argparse
import asyncio
import json
import shlex
import os
import signal
from pathlib import Path
from functools import partial

from capstone import Cs, CS_ARCH_ARM64, CS_MODE_ARM
from rich.text import Text
from rich.console import Group
from rich.syntax import Syntax
from textual import on, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.widgets import Button, DataTable, Footer, Header, Input, Label, RichLog, Static, TabbedContent, TabPane

from .client import Client
from .analysis import prepare_rva_image


class KernelApp(App):
    TITLE = 'vroom / kernel'
    SUB_TITLE = 'Windows ARM64'
    CSS = """
    Screen { background: #101720; color: #d7e0eb; }
    Header { background: #182431; }
    #status { height: 1; padding: 0 2; background: #182431; color: #72d6cb; }
    #workspace { height: 1fr; }
    #execution-toolbar { height: 1; background: #101720; }
    #execution-toolbar Button { min-width: 10; width: auto; height: 1; margin-right: 1; border: none; background: #182431; }
    #execution-toolbar Button:hover { background: #234a64; }
    #run-button { color: #72d6cb; }
    #pause-button { color: #e9b65d; }
    #execution-toolbar Button:disabled { color: #526272; }
    #module-filter { height: 3; border: solid #2c4054; }
    #memory-table, #breakpoints { height: 1fr; background: #101720; }
    #cfg-title { height: 1; padding: 0 1; color: #72d6cb; }
    #cfg-split { height: 1fr; }
    #cfg-blocks, #cfg-edges { width: 1fr; height: 1fr; background: #101720; }
    #cfg-edges { border-left: solid #2c4054; }
    #status.error { color: #ff8080; }
    #analysis-source { border: none; background: #234a64; }
    Screen.short #execution-toolbar { display: none; }
    #sidebar { width: 30; border-right: solid #2c4054; }
    .section { height: 1; padding: 0 1; color: #7fa7ce; text-style: bold; }
    #modules { height: 1fr; background: #101720; }
    #callbacks { height: 1fr; background: #101720; }
    #center { width: 1fr; }
    #register-pane { width: 28; border-left: solid #2c4054; }
    #registers { height: 1fr; background: #101720; }
    #code-split { height: 1fr; }
    #disassembly-pane, #decompiler-pane { width: 1fr; min-width: 24; }
    #decompiler-pane { border-left: solid #2c4054; }
    #code, #decompiler { padding: 0 1; width: auto; min-width: 100%; }
    #code-scroll, #decompiler-scroll { height: 1fr; overflow-x: auto; }
    #analysis-toolbar { height: 3; }
    #analysis-source { min-width: 8; width: 8; height: 3; margin: 0 1; }
    #analysis-status { width: 1fr; height: 3; content-align: left middle; }
    #code-title { height: 3; padding: 1; color: #72d6cb; }
    #process-event { height: 1fr; background: #101720; }
    #events { height: 4; border-top: solid #2c4054; background: #0c121a; padding: 0 1; }
    #command { dock: bottom; height: 3; border: solid #38516b; }
    Footer { background: #182431; }
    DataTable > .datatable--cursor { background: #234a64; color: #ffffff; }
    Screen.narrow #sidebar, Screen.narrow #register-pane { display: none; }
    Screen.narrow.show-sidebar #sidebar { display: block; width: 32; }
    Screen.narrow.show-registers #register-pane { display: block; width: 30; }
    Screen.short #events { height: 4; }
    Screen.short .section, Screen.short #status { height: 1; }
    Screen.code-focus #sidebar, Screen.code-focus #register-pane,
    Screen.narrow.code-focus #sidebar, Screen.narrow.code-focus #register-pane { display: none; }
    Screen.code-focus #events { height: 3; }
    """
    BINDINGS = [
        Binding('f4', 'discover', 'Windows'), Binding('f5', 'resume', 'Run'),
        Binding('f6', 'pause', 'Pause'), Binding('f7', 'step', 'Into'),
        Binding('f8', 'step_over', 'Over'), Binding('shift+f8', 'step_out', 'Out'),
        Binding('f2', 'breakpoint', 'BP'), Binding('colon', 'command', 'Command'),
        Binding('f9', 'panels', 'Panels'),
        Binding('f3', 'decompile', 'Decompile'), Binding('f10', 'code_focus', 'Code focus'),
        Binding('u', 'navigation_back', 'Back'), Binding('shift+u', 'navigation_forward', 'Forward'),
        Binding('x', 'xrefs', 'Xrefs'), Binding('ctrl+g', 'cfg', 'CFG', show=False),
        Binding('ctrl+q', 'poweroff', 'Quit'),
        Binding('up', 'history_previous', show=False), Binding('down', 'history_next', show=False),
    ]

    def __init__(self, binary=None, config=None, client=None, atui=None, restore=None, ex_callback_demo=False):
        super().__init__()
        self.client = client or Client(binary, config, self.event, self.log_message, restore=restore)
        self.cpu = 0
        self.address = 0
        self.breakpoints = []
        self.modules = []
        self.callbacks = []
        self.processes = []
        self.functions = []
        self.function_base = None
        self.function_offset = 0
        self.function_more = False
        self.function_timer = None
        self.context = None
        self.watchpoints = []
        self.analysis_cache = {}
        self.cfg_cache = {}
        self.generation = 0
        self.view_revision = 0
        self.view_context = True
        self.analysis_mode = 'live'
        self.cpu_ttbr0 = None
        self.analysis_lock = asyncio.Lock()
        self.register_values = {}
        self.register_cpu = None
        self.current_pc = None
        self.memory_address = None
        self.memory_length = 256
        self.memory_physical = False
        self.history = []
        self.history_index = 0
        self.navigation_history = []
        self.navigation_position = -1
        self.execution_lock = asyncio.Lock()
        self.command_lock = asyncio.Lock()
        self.running = False
        self.ready = False
        self.decoder = Cs(CS_ARCH_ARM64, CS_MODE_ARM)
        self.atui = atui
        self.ex_callback_demo = ex_callback_demo

    def compose(self) -> ComposeResult:
        yield Header()
        yield Static('Connecting…', id='status')
        with Horizontal(id='execution-toolbar'):
            yield Button('F5 Run', id='run-button', disabled=True)
            yield Button('F6 Pause', id='pause-button', disabled=True)
            yield Button('F7 Into', id='into-button', disabled=True)
            yield Button('F8 Over', id='over-button', disabled=True)
            yield Button('S-F8 Out', id='out-button', disabled=True)
        with Horizontal(id='workspace'):
            with Vertical(id='sidebar'):
                yield Label('MODULES', classes='section')
                yield Input(placeholder='Filter modules', id='module-filter')
                yield DataTable(id='modules', cursor_type='row')
                yield Label('PROCESS CALLBACKS', classes='section')
                yield DataTable(id='callbacks', cursor_type='row')
            with Vertical(id='center'):
                with TabbedContent():
                    with TabPane('Code', id='code-tab'):
                        with Horizontal(id='code-split'):
                            with Vertical(id='disassembly-pane'):
                                yield Static('DISASSEMBLY · LIVE', id='code-title')
                                with VerticalScroll(id='code-scroll'):
                                    yield Static('', id='code')
                            with Vertical(id='decompiler-pane'):
                                with Horizontal(id='analysis-toolbar'):
                                    yield Static('DECOMPILE', id='analysis-status')
                                    yield Button('Live', id='analysis-source')
                                with VerticalScroll(id='decompiler-scroll'):
                                    yield Static('', id='decompiler')
                    with TabPane('Functions', id='functions-tab'):
                        yield Input(placeholder='Function prefix', id='function-filter')
                        yield DataTable(id='functions', cursor_type='row')
                    with TabPane('Processes', id='processes-tab'):
                        yield DataTable(id='processes', cursor_type='row')
                    with TabPane('PEB', id='peb-tab'):
                        yield DataTable(id='peb', cursor_type='row')
                    with TabPane('Stack', id='stack-tab'):
                        yield DataTable(id='stack', cursor_type='row')
                    with TabPane('Callback event', id='callback-tab'):
                        yield DataTable(id='process-event', cursor_type='row')
                    with TabPane('Memory', id='memory-tab'):
                        yield DataTable(id='memory-table', cursor_type='row')
                    with TabPane('Breakpoints', id='breakpoints-tab'):
                        yield DataTable(id='breakpoints', cursor_type='row')
                    with TabPane('CFG', id='cfg-tab'):
                        yield Static('', id='cfg-title')
                        with Horizontal(id='cfg-split'):
                            yield DataTable(id='cfg-blocks', cursor_type='row')
                            yield DataTable(id='cfg-edges', cursor_type='row')
            with Vertical(id='register-pane'):
                yield Label('REGISTERS', classes='section')
                yield DataTable(id='registers', cursor_type='row', show_header=False, cell_padding=0)
        yield RichLog(id='events', wrap=True, markup=False)
        yield Input(placeholder=':', id='command')
        yield Footer()

    async def on_mount(self):
        self.query_one('#registers', DataTable).add_column('Register', width=7)
        self.query_one('#registers', DataTable).add_column('Value', width=16)
        self.query_one('#memory-table', DataTable).add_columns('Address', 'Bytes', 'ASCII')
        self.query_one('#breakpoints', DataTable).add_columns('Type', 'Address', 'Access', 'Scope')
        self.query_one('#cfg-blocks', DataTable).add_columns('Block', 'Size', 'Insns', 'In', 'Out')
        self.query_one('#cfg-edges', DataTable).add_columns('From', 'Kind', 'To')
        self.query_one('#functions', DataTable).add_columns('Function', 'Address')
        self.query_one('#processes', DataTable).add_columns('PID', 'Image', 'Parent', 'EPROCESS')
        self.query_one('#peb', DataTable).add_columns('Field', 'Value')
        self.query_one('#stack', DataTable).add_columns('#', 'PC', 'SP', 'Symbol', 'Unwind')
        self.query_one('#modules', DataTable).add_columns('Module', 'Size')
        self.query_one('#callbacks', DataTable).add_columns('Slot', 'Owner / offset')
        self.query_one('#process-event', DataTable).add_columns('Field', 'Value')
        try:
            await self.client.start()
        except (OSError, RuntimeError) as error:
            self.status(f'FAILED  /  {error}')
            self.log_message(error)

    @on(TabbedContent.TabActivated)
    @work(group='tab-load', exclusive=True)
    async def load_tab(self, event):
        if not self.ready or self.running or event.pane.id != self.query_one(TabbedContent).active:
            return
        try:
            if event.pane.id == 'functions-tab': self.refresh_functions()
            elif event.pane.id == 'processes-tab': await self.load_processes(activate=False)
            elif event.pane.id == 'peb-tab' and (self.context or {}).get('pid'): await self.show_peb(activate=False)
            elif event.pane.id == 'stack-tab': await self.show_stack(activate=False)
        except Exception as error: self.log_message(error)

    def on_resize(self, event):
        self.screen.set_class(event.size.width < 140, 'narrow')
        self.screen.set_class(event.size.height < 35, 'short')

    def action_code_focus(self):
        self.screen.toggle_class('code-focus')
        self.query_one(TabbedContent).active = 'code-tab'

    def invalidate_code(self, reason='', clear_code=True):
        self.view_revision += 1
        self.workers.cancel_group(self, 'decompile')
        self.query_one('#decompiler', Static).update('')
        self.query_one('#analysis-status', Static).update(reason or 'DECOMPILE')
        if clear_code:
            self.query_one('#code', Static).update('')

    def owner(self, address, context=True):
        if not context and address < (1 << 63) and (self.context or {}).get('pid'):
            table = self.context.get('directory_table')
            if table is None or self.cpu_ttbr0 is None or table & 0x0000fffffffff000 != self.cpu_ttbr0 & 0x0000fffffffff000:
                return None
        return next((m for m in self.modules if m['base'] <= address < m['base'] + m['size']), None)

    def action_decompile(self):
        self.run_worker(self.show_address(self.address, context=self.view_context), group='browse', exclusive=True)

    @on(Button.Pressed, '#analysis-source')
    def switch_analysis_source(self):
        self.analysis_mode = 'file' if self.analysis_mode == 'live' else 'live'
        self.query_one('#analysis-source', Button).label = self.analysis_mode.title()
        self.action_decompile()

    def action_panels(self):
        self.screen.remove_class('code-focus')
        if not self.screen.has_class('narrow'):
            self.query_one('#modules', DataTable).focus()
        elif self.screen.has_class('show-sidebar'):
            self.screen.remove_class('show-sidebar')
            self.screen.add_class('show-registers')
        elif self.screen.has_class('show-registers'):
            self.screen.remove_class('show-registers')
        else:
            self.screen.add_class('show-sidebar')
            self.query_one('#modules', DataTable).focus()

    def log_message(self, message):
        self.query_one('#events', RichLog).write(str(message))

    def status(self, text, error=False):
        if text.startswith('PAUSED') and (self.context or {}).get('pid') and 'PID ' not in text:
            text += f" / PID {self.context['pid']}"
        self.query_one('#status', Static).set_class(error, 'error')
        self.query_one('#status', Static).update(text)

    def event(self, event):
        kind = event['event']
        if kind in ('ready', 'stopped', 'fault', 'reset_requested'):
            self.invalidate_code()
            self.generation += 1
            self.analysis_cache.clear()
            self.cfg_cache.clear()
            self.query_one('#memory-table', DataTable).clear()
            self.memory_address = None
            self.ready = True
            self.running = False
            self.cpu = event.get('cpu', self.cpu)
            self.status(f"PAUSED  /  CPU {self.cpu}  /  {event.get('reason', kind)}")
            self.refresh_state()
            if kind == 'ready' and self.ex_callback_demo:
                self.start_demo()
        elif kind in ('shutdown', 'disconnected'):
            self.invalidate_code('DISCONNECTED')
            self.ready = False
            self.running = False
            self.status('DISCONNECTED  /  backend stopped')
        self.sync_controls()
        if kind != 'ready': self.log_message(f"{kind} · {event.get('reason', event.get('message', ''))}")

    @work(group='demo', exclusive=True)
    async def start_demo(self):
        try:
            await self.action_discover().wait()
            await self.load_callbacks()
            regs = await self.client.call('registers', cpu=self.cpu)
            index = next((i for i, callback in enumerate(self.callbacks)
                          if callback['function'] == regs['pc']), None)
            if index is None:
                raise RuntimeError('Demo needs a checkpoint stopped at a registered Ex callback entry')
            callback = self.callbacks[index]
            self.query_one('#callbacks', DataTable).move_cursor(row=index)
            await self.show_address(regs['pc'])
            await self.show_process_event()
            self.query_one(TabbedContent).active = 'code-tab'
            self.status(f"PAUSED  /  {callback['module'] or 'callback'}  /  process notification  /  CPU {self.cpu}")
        except Exception as error:
            self.log_message(error)

    async def show_process_event(self):
        event = await self.client.call('process_event', cpu=self.cpu)
        rows = [('Event', event['kind']), ('PID', str(event['pid'])),
                ('EPROCESS', f"0x{event['process']:016x}")]
        if event['kind'] == 'create':
            rows += [('Parent PID', str(event['parent_pid'])),
                     ('Creator PID', str(event['creator_pid'])),
                     ('Creator TID', str(event['creator_tid'])),
                     ('Image', event['image'] or '—'),
                     ('Command line', event['command_line'] or '—'),
                     ('CreationStatus', f"0x{event['creation_status']:08x}")]
        table = self.query_one('#process-event', DataTable)
        table.clear()
        table.add_rows(rows)
        self.query_one(TabbedContent).active = 'callback-tab'

    @work(exclusive=True, group='refresh')
    async def refresh_state(self):
        revision = self.view_revision
        try:
            regs = await self.client.call('registers', cpu=self.cpu)
            if revision != self.view_revision: return
            self.render_registers(regs)
            self.cpu_ttbr0 = regs.get('ttbr0')
            await self.show_address(regs['pc'], context=False, activate=False, record=False)
        except Exception as error:
            self.log_message(error)

    async def disassemble(self, address, context=True, revision=None):
        if revision is None: revision = self.view_revision
        data = await self.client.call('read_virtual', cpu=self.cpu, address=address, length=128, context=context)
        if revision != self.view_revision: return
        text = Text(no_wrap=True, overflow='ignore')
        owner = self.owner(address, context)
        title = f"{owner['name']} + 0x{address-owner['base']:x}" if owner else f'0x{address:016x}'
        text.append(title + '\n\n', style='bold #72d6cb')
        for instruction in self.decoder.disasm(bytes.fromhex(data['hex']), address):
            mark = '*' if instruction.address in self.breakpoints else '>' if instruction.address == self.current_pc else ' '
            text.append(f'{mark} {instruction.address:016x}  ', style='#69839c')
            text.append(f'{instruction.mnemonic:<9}', style='#72d6cb')
            text.append(instruction.op_str + '\n', style='#d7e0eb')
        self.query_one('#code', Static).styles.width = max(line.cell_len for line in text.split('\n')) + 2
        self.query_one('#code', Static).update(text)

    def sync_controls(self):
        for name in ('run', 'into', 'over', 'out'):
            self.query_one(f'#{name}-button', Button).disabled = not self.ready or self.running
        self.query_one('#pause-button', Button).disabled = not self.ready

    @on(Button.Pressed)
    def execution_button(self, event):
        actions = {'run-button':self.action_resume, 'pause-button':self.action_pause,
                   'into-button':self.action_step, 'over-button':self.action_step_over, 'out-button':self.action_step_out}
        if event.button.id in actions: actions[event.button.id]()

    async def execute(self, operation, vector_length=None):
        if not self.ready or (self.execution_lock.locked() and operation != 'pause'): return
        async with self.execution_lock:
            if self.running and operation != 'pause': return
            generation = self.generation
            self.invalidate_code('PAUSING' if operation == 'pause' else 'RUNNING')
            self.running = operation != 'pause'
            if self.running:
                self.query_one('#memory-table', DataTable).clear()
                self.memory_address = None
            self.sync_controls()
            try:
                await self.client.call(operation, cpu=self.cpu, **({'vector_length':vector_length} if vector_length is not None else {}))
                if generation == self.generation:
                    self.running = operation != 'pause'
                    self.status(f"{'PAUSED' if operation == 'pause' else operation.upper().replace('_', ' ')} / CPU {self.cpu}")
                    if operation == 'pause': self.refresh_state()
            except Exception as error:
                self.running = False
                self.status(str(error), error=True)
                self.log_message(error)
                self.refresh_state()
            finally:
                self.sync_controls()

    @work(group='control')
    async def action_pause(self): await self.execute('pause')

    @work(group='control')
    async def action_resume(self): await self.execute('resume')

    @work(group='control')
    async def action_step(self): await self.execute('step')

    @work(group='control')
    async def action_step_over(self): await self.execute('step_over')

    @work(group='control')
    async def action_step_out(self): await self.execute('step_out')

    def render_registers(self, regs):
        self.register_cpu = self.cpu
        values = {name:regs[name] for name in ('pc','sp','pstate')}
        values.update({f'x{i}':v for i,v in enumerate(regs['x'])})
        values.update({name:regs[name] for name in ('ttbr0','ttbr1','tcr','sctlr','vbar') if name in regs})
        previous = self.register_values.get(self.cpu, {})
        table = self.query_one('#registers', DataTable)
        row = table.cursor_row
        table.clear()
        for name,value in values.items():
            changed = name in previous and previous[name] != value
            table.add_row(name.upper(), Text(f'{value:016x}', style='bold #e9b65d' if changed else '#d7e0eb'), key=name)
        table.move_cursor(row=min(row,len(values)-1))
        self.register_values[self.cpu] = values
        self.current_pc = regs['pc']

    def edit_command(self, text):
        command = self.query_one('#command', Input)
        command.value = text
        command.focus()
        command.cursor_position = len(text)

    def action_history_previous(self):
        if self.focused is self.query_one('#command') and self.history:
            self.history_index = max(0, self.history_index-1)
            self.edit_command(self.history[self.history_index])

    def action_history_next(self):
        if self.focused is self.query_one('#command') and self.history:
            self.history_index = min(len(self.history), self.history_index+1)
            self.edit_command(self.history[self.history_index] if self.history_index < len(self.history) else '')

    def remember_address(self, address, context):
        entry = (address, context)
        if self.navigation_position >= 0 and self.navigation_history[self.navigation_position] == entry:
            return
        del self.navigation_history[self.navigation_position + 1:]
        self.navigation_history.append(entry)
        self.navigation_history = self.navigation_history[-128:]
        self.navigation_position = len(self.navigation_history) - 1

    def navigate_history(self, delta):
        position = self.navigation_position + delta
        if not 0 <= position < len(self.navigation_history):
            return
        self.navigation_position = position
        address, context = self.navigation_history[position]
        self.run_worker(self.show_address(address, context=context, record=False),
                        group='browse', exclusive=True)

    def action_navigation_back(self):
        self.navigate_history(-1)

    def action_navigation_forward(self):
        self.navigate_history(1)

    def render_breakpoints(self):
        table = self.query_one('#breakpoints', DataTable)
        table.clear()
        for address in self.breakpoints:
            table.add_row('Exec', f'{address:016x}', 'x', 'kernel' if address >> 63 else 'process', key=f'{address}')
        for watch in self.watchpoints:
            table.add_row(f"Data {watch['size']}B", f"{watch['address']:016x}", watch['mode'], 'all CPUs')

    async def show_memory(self, address, length=256, physical=False):
        result = await self.client.call('read_physical' if physical else 'read_virtual', address=address, cpu=self.cpu, length=length)
        data = bytes.fromhex(result['hex'])
        table = self.query_one('#memory-table', DataTable)
        table.clear()
        for i in range(0,len(data),16):
            chunk = data[i:i+16]
            table.add_row(f'{address+i:016x}', chunk.hex(' '), ''.join(chr(b) if 32 <= b < 127 else '.' for b in chunk), key=str(address+i))
        self.memory_address, self.memory_length, self.memory_physical = address, length, physical
        self.query_one(TabbedContent).active = 'memory-tab'

    @work(group='control', exclusive=True)
    async def action_discover(self):
        try:
            await self.client.call('pause')
            self.status('WINDOWS  /  discovering kernel and loading matching symbols…')
            self.running = False
            image = await self.client.call('kernel', cpu=self.cpu)
            self.log_message(f"{image['name']} at 0x{image['base']:016x}")
            symbols = await self.client.call('symbols')
            self.log_message(f"Verified PDB: {symbols['count']} symbols")
            await self.load_modules()
            self.status(f"PAUSED  /  {image['name']}  /  {len(self.modules)} modules  /  symbols verified")
            self.refresh_state()
        except Exception as error:
            self.status('PAUSED  /  Windows discovery needs attention')
            self.log_message(error)

    async def load_modules(self):
        self.modules = await self.client.call('modules', cpu=self.cpu)
        if (self.context or {}).get('pid'):
            try: self.modules += await self.client.call('user_modules', cpu=self.cpu)
            except Exception as error: self.log_message(error)
        self.filter_modules()

    @on(Input.Changed, '#module-filter')
    def filter_modules(self):
        query = self.query_one('#module-filter', Input).value.casefold()
        table = self.query_one('#modules', DataTable)
        table.clear()
        for i, module in enumerate(self.modules):
            if query in module['name'].casefold():
                table.add_row(module['name'], f"{module['size']//1024}K", key=str(i))

    async def load_processes(self, activate=True):
        self.processes = await self.client.call('processes', cpu=self.cpu)
        table = self.query_one('#processes', DataTable)
        table.clear()
        for i, p in enumerate(self.processes):
            table.add_row(str(p['pid']), p['name'], str(p['parent_pid']), f"{p['eprocess']:016x}", key=str(i))
        if activate: self.query_one(TabbedContent).active = 'processes-tab'

    async def select_process(self, pid=None):
        try:
            self.invalidate_code()
            self.navigation_history.clear()
            self.navigation_position = -1
            self.query_one('#memory-table', DataTable).clear()
            self.memory_address = None
            self.context = await self.client.call('process', cpu=self.cpu, **({'pid':pid} if pid is not None else {}))
            await self.load_modules()
            if pid is not None:
                self.status(f"PAUSED / CPU {self.cpu} / PID {pid} / {self.context['name']}")
                await self.show_peb()
            else:
                self.status(f'PAUSED / CPU {self.cpu}')
        except Exception as error:
            self.log_message(error)

    async def show_peb(self, activate=True):
        peb = await self.client.call('peb', cpu=self.cpu)
        table = self.query_one('#peb', DataTable)
        table.clear()
        table.add_rows((key, hex(value) if isinstance(value, int) else str(value or '—')) for key, value in peb.items())
        if activate: self.query_one(TabbedContent).active = 'peb-tab'

    async def symbol_address(self, name):
        try: return int(name, 0)
        except ValueError: pass
        if '!' in name:
            module, name = name.split('!', 1)
            if module.lower() not in ('nt', 'ntoskrnl', 'ntoskrnl.exe'):
                base = self.module_named(module)['base']
                await self.client.call('symbols', cpu=self.cpu, base=base)
                return await self.client.call('resolve', base=base, name=name)
        return await self.client.call('resolve', name=name)

    def module_named(self, name):
        module = next((m for m in self.modules if m['name'].lower() == name.lower()
                       or Path(m['name']).stem.lower() == name.lower()), None)
        if module is None: raise ValueError(f'Module not loaded: {name}')
        return module

    @on(Input.Changed, '#function-filter')
    def filter_functions(self, event):
        if self.function_timer: self.function_timer.stop()
        self.function_offset = 0
        self.function_timer = self.set_timer(.12, self.refresh_functions)

    @work(group='functions', exclusive=True)
    async def refresh_functions(self):
        try:
            args = {'base':self.function_base} if self.function_base is not None else {}
            result = await self.client.call('functions', prefix=self.query_one('#function-filter', Input).value,
                                            offset=self.function_offset, limit=200, **args)
            self.functions = result['rows']
            self.function_more = result['more']
            table = self.query_one('#functions', DataTable)
            table.clear()
            for i, f in enumerate(self.functions):
                table.add_row(f['name'], f"{f['address']:016x}", key=str(i))
        except Exception as error: self.log_message(error)

    async def show_stack(self, activate=True, vector_length=None):
        result = await self.client.call('stack', cpu=self.cpu, **({'vector_length':vector_length} if vector_length is not None else {}))
        table = self.query_one('#stack', DataTable)
        table.clear()
        for i, f in enumerate(result['frames']):
            table.add_row(str(i), f"{f['pc']:016x}", f"{f['sp']:016x}", f.get('symbol') or '', f['method'])
        if result.get('stop'): self.log_message(result['stop'])
        if activate: self.query_one(TabbedContent).active = 'stack-tab'

    async def load_callbacks(self):
        self.callbacks = await self.client.call('callbacks', cpu=self.cpu)
        table = self.query_one('#callbacks', DataTable)
        table.clear()
        for i, callback in enumerate(self.callbacks):
            owner = callback['module'] or 'unresolved'
            offset = callback['module_offset']
            table.add_row(str(callback['slot']), f"{owner}+0x{offset:x}" if offset is not None else owner, key=str(i))
        self.log_message(f'callbacks: {len(self.callbacks)}')
        for source in sorted({c.get('layout_source', 'fixture') for c in self.callbacks}):
            self.log_message('Layout evidence: ' + source)

    @work(group='control', exclusive=True)
    async def action_breakpoint(self):
        address = self.address
        table = self.focused
        if isinstance(table, DataTable) and table.row_count and table.id in ('callbacks','functions'):
            index = int(table.ordered_rows[table.cursor_row].key.value)
            address = self.callbacks[index]['function'] if table.id == 'callbacks' else self.functions[index]['address']
        try: await self.toggle_breakpoint(address)
        except Exception as error: self.log_message(error)

    async def toggle_breakpoint(self, address):
        addresses = list(self.breakpoints)
        if address in addresses: addresses.remove(address)
        else: addresses.append(address)
        self.breakpoints = await self.client.call('breakpoints', addresses=addresses)
        self.render_breakpoints()
        self.log_message('Hardware breakpoints: ' + ', '.join(f'0x{x:x}' for x in self.breakpoints))
        if address == self.address:
            await self.disassemble(address, context=self.view_context)

    def action_command(self):
        self.query_one('#command', Input).focus()

    @on(DataTable.RowSelected)
    def selected(self, event):
        if event.data_table.id == 'registers':
            if self.register_cpu != self.cpu: return
            name = str(event.row_key.value)
            self.edit_command(f"reg {name} 0x{self.register_values[self.cpu][name]:x}")
            return
        if event.data_table.id == 'memory-table':
            row = event.data_table.get_row(event.row_key)
            self.edit_command(f"{'write-phys' if self.memory_physical else 'write'} 0x{int(event.row_key.value):x} {row[1].replace(' ', '')}")
            return
        if event.data_table.id == 'breakpoints':
            address = int(event.data_table.get_row(event.row_key)[1],16)
            kind = event.data_table.get_row(event.row_key)[0]
            self.edit_command(f"{'wc' if str(kind).startswith('Data') else 'bc'} 0x{address:x}")
            return
        if event.data_table.id == 'cfg-blocks':
            address = int(event.row_key.value)
            self.run_worker(self.show_address(address), group='browse', exclusive=True)
            return
        if event.data_table.id == 'cfg-edges':
            address = int(event.data_table.get_row(event.row_key)[2], 16)
            self.run_worker(self.show_address(address), group='browse', exclusive=True)
            return
        if event.data_table.id == 'stack':
            address = int(event.data_table.get_row(event.row_key)[1],16)
            self.run_worker(self.show_address(address,context=False), group='browse', exclusive=True)
            return
        if event.data_table.id == 'processes':
            pid = self.processes[int(event.row_key.value)]['pid']
            self.run_worker(self.select_process(pid), group='browse', exclusive=True)
            return
        if event.data_table.id == 'functions':
            self.run_worker(self.show_address(self.functions[int(event.row_key.value)]['address']), group='browse', exclusive=True)
            return
        if event.data_table.id not in ('modules', 'callbacks'):
            return
        index = int(event.row_key.value)
        if event.data_table.id == 'modules':
            self.run_worker(self.browse_module(self.modules[index]['base']), group='browse', exclusive=True)
        else:
            self.run_worker(self.show_address(self.callbacks[index]['function']), group='browse', exclusive=True)

    @on(Input.Submitted, '#function-filter')
    def open_filtered_function(self):
        if self.functions:
            self.run_worker(self.show_address(self.functions[0]['address']), group='browse', exclusive=True)

    @on(Input.Submitted, '#module-filter')
    def open_filtered_module(self):
        table = self.query_one('#modules', DataTable)
        if table.row_count:
            index = int(table.ordered_rows[0].key.value)
            self.run_worker(self.browse_module(self.modules[index]['base']), group='browse', exclusive=True)

    async def browse_module(self, base):
        try:
            self.function_base = base
            self.function_offset = 0
            await self.client.call('symbols', cpu=self.cpu, base=base)
            self.query_one('#function-filter', Input).value = ''
            self.query_one(TabbedContent).active = 'functions-tab'
            self.refresh_functions()
            self.query_one('#function-filter', Input).focus()
        except Exception as error:
            self.status(str(error), error=True)
            self.log_message(error)

    async def show_address(self, address, context=True, activate=True, analyze=True, allow_missing=True,
                           file_image=None, record=True):
        self.invalidate_code()
        revision = self.view_revision
        self.address, self.view_context = address, context
        if record:
            self.remember_address(address, context)
        elif not self.navigation_history:
            self.remember_address(address, context)
        if activate: self.query_one(TabbedContent).active = 'code-tab'
        self.query_one('#code-scroll', VerticalScroll).scroll_home(animate=False)
        self.query_one('#decompiler-scroll', VerticalScroll).scroll_home(animate=False)
        try:
            await self.disassemble(address, context=context, revision=revision)
        except Exception as error:
            if revision == self.view_revision:
                self.query_one('#code', Static).update(Text(str(error)))
        if revision != self.view_revision or not analyze: return
        if file_image is None: file_image = self.analysis_mode == 'file'
        self.run_worker(partial(self.decompile, address, allow_missing=allow_missing, file_image=file_image,
                                       revision=revision, context=context), group='decompile', exclusive=True)

    @on(Input.Submitted, '#command')
    @work(group='commands')
    async def command(self, event):
        event.input.value = ''
        async with self.command_lock:
            await self.submit_command(event)

    async def submit_command(self, event):
        if event.value.strip() and (not self.history or self.history[-1] != event.value):
            self.history.append(event.value)
            self.history = self.history[-100:]
        self.history_index = len(self.history)
        try:
            words = shlex.split(event.value.lstrip(':').strip())
            if not words: return
            op, *args = words
            if op in ('reg', 'write', 'write-phys'):
                async with self.execution_lock:
                    if self.running: raise RuntimeError('Pause before editing')
                    if op == 'reg':
                        regs = await self.client.call('write_register', cpu=self.cpu, register=args[0], value=int(args[1],0))
                        self.render_registers(regs)
                    else:
                        address = await self.symbol_address(args[0])
                        data = bytes.fromhex(''.join(args[1:]))
                        await self.client.call('write_physical' if op == 'write-phys' else 'write_virtual', cpu=self.cpu, address=address, hex=data.hex())
                    self.generation += 1
                    self.analysis_cache.clear()
                    self.cfg_cache.clear()
                    self.invalidate_code()
                    if op == 'reg': await self.show_address(regs['pc'], context=False)
                    elif self.memory_address is not None: await self.show_memory(self.memory_address,self.memory_length,self.memory_physical)
                    else: await self.show_address(self.address,context=self.view_context)
                    self.status(f'PAUSED / CPU {self.cpu} / updated')
            elif op in ('go', 'into', 'over', 'out'):
                await self.execute({'go':'resume', 'into':'step', 'over':'step_over', 'out':'step_out'}[op], vector_length=int(args[0],0) if args and op == 'out' else None)
            elif op == 'callbacks': await self.load_callbacks()
            elif op == 'event':
                await self.show_process_event()
            elif op == 'modules': await self.load_modules()
            elif op == 'ps': await self.load_processes()
            elif op == 'process': await self.select_process(int(args[0], 0) if args else None)
            elif op == 'peb': await self.show_peb()
            elif op == 'stack': await self.show_stack(vector_length=int(args[0],0) if args else None)
            elif op in ('symbols', 'functions'):
                self.function_base = self.module_named(args[0])['base'] if args else None
                await self.client.call('symbols', cpu=self.cpu, **({'base':self.function_base} if self.function_base is not None else {}))
                if op == 'functions':
                    self.function_offset = 0
                    self.query_one('#function-filter', Input).value = args[1] if len(args)>1 else ''
                    self.refresh_functions()
                    self.query_one(TabbedContent).active = 'functions-tab'
                    self.query_one('#function-filter', Input).focus()
            elif op in ('next', 'prev'):
                if op == 'prev': self.function_offset = max(0, self.function_offset - 200)
                elif self.function_more: self.function_offset += 200
                self.refresh_functions()
            elif op == 'watch':
                watch = {'address':await self.symbol_address(args[0]), 'size':int(args[1],0), 'mode':args[2]}
                self.watchpoints = await self.client.call('watchpoints', watches=[*self.watchpoints, watch])
                self.render_breakpoints()
            elif op == 'wc':
                remove = await self.symbol_address(args[0]) if args else None
                self.watchpoints = await self.client.call('watchpoints', watches=[w for w in self.watchpoints if w['address'] != remove] if args else [])
                self.render_breakpoints()
            elif op == 'image':
                module = self.module_named(args[0])
                exported = await self.client.call('image', cpu=self.cpu, base=module['base'], **({'path':args[1]} if len(args)>1 else {}))
                self.log_message(exported['source'])
            elif op == 'cpu':
                self.cpu = int(args[0]); self.refresh_state()
            elif op == 'bc':
                remove = await self.symbol_address(args[0]) if args else None
                self.breakpoints = await self.client.call('breakpoints', addresses=[a for a in self.breakpoints if a != remove] if args else [])
                self.render_breakpoints()
            elif op in ('dec', 'dec-file', 'dec-partial'):
                address = await self.symbol_address(args[0]) if args else self.address
                self.analysis_mode = 'file' if op == 'dec-file' else 'live'
                self.query_one('#analysis-source', Button).label = self.analysis_mode.title()
                await self.show_address(address, allow_missing=op != 'dec', file_image=op == 'dec-file')
            elif op == 'cfg':
                address = await self.symbol_address(args[0]) if args else self.address
                await self.show_cfg(address)
            elif op in ('back', 'forward'):
                self.navigate_history(-1 if op == 'back' else 1)
            elif op == 'xrefs':
                address = await self.symbol_address(args[0]) if args else self.address
                await self.show_cfg(address, xrefs=True)
            elif op in ('bp', 'dis', 'mem', 'mem-phys', 'x'):
                address = await self.symbol_address(args[0])
                if op == 'bp': await self.toggle_breakpoint(address)
                elif op == 'dis': await self.show_address(address)
                elif op == 'x': self.log_message(f'{args[0]} = 0x{address:016x}')
                else:
                    await self.show_memory(address, int(args[1],0) if len(args)>1 else 256, op == 'mem-phys')
            elif op == 'dump': self.log_message(await self.client.call('dump', cpu=self.cpu, address=await self.symbol_address(args[0]), length=int(args[1],0)))
            elif op == 'checkpoint': self.log_message(await self.client.call('checkpoint', path=args[0]))
            else: self.log_message('reg NAME VALUE · write ADDRESS HEX · into/over/out · ps · process PID · peb · modules · symbols MODULE · functions MODULE PREFIX · next/prev · stack · bp/dis/mem/x SYMBOL · watch ADDRESS SIZE r/w/rw · bc/wc · dec/dec-file/dec-partial SYMBOL · cfg/xrefs [SYMBOL] · back/forward · image MODULE [PATH] · dump ADDRESS N · callbacks · event · cpu N · checkpoint PATH')
        except Exception as error:
            self.status(str(error), error=True)
            self.log_message(error)

    @work(group='exit', exclusive=True)
    async def action_poweroff(self):
        await self.client.close()
        self.exit()

    async def on_unmount(self):
        await self.client.close()

    def cfg_address(self):
        table = self.focused
        if isinstance(table, DataTable) and table.id == 'cfg-blocks' and table.row_count:
            return int(table.ordered_rows[table.cursor_row].key.value)
        return self.address

    @work(group='cfg', exclusive=True)
    async def action_cfg(self):
        await self.show_cfg(self.cfg_address())

    @work(group='cfg', exclusive=True)
    async def action_xrefs(self):
        await self.show_cfg(self.cfg_address(), xrefs=True)

    async def run_angr_cfg(self, path, rva):
        output = await self.run_atui(path, '--pd', hex(rva), '--json')
        try:
            cfg = json.loads(output)
        except json.JSONDecodeError as error:
            raise RuntimeError('angr returned invalid CFG data') from error
        if not isinstance(cfg, dict) or not isinstance(cfg.get('blocks'), list) or not isinstance(cfg.get('edges'), list):
            raise RuntimeError('angr returned incomplete CFG data')
        if len(cfg['blocks']) > 100_000 or len(cfg['edges']) > 500_000:
            raise RuntimeError('angr CFG is too large to display')
        return cfg

    def cached_cfg(self, module, address, context, file_image):
        prefix = (self.generation, (self.context or {}).get('pid'), self.cpu,
                  context, module['base'], file_image)
        for key, cfg in self.cfg_cache.items():
            if key[:6] != prefix:
                continue
            rva = address - module['base']
            if any(block['addr'] <= rva < block['addr'] + max(1, block['size'])
                   for block in cfg['blocks']):
                return cfg
        return None

    async def show_cfg(self, address, xrefs=False, context=None, file_image=None):
        if context is None:
            context = self.view_context
        if file_image is None:
            file_image = self.analysis_mode == 'file'
        generation, pid, cpu = self.generation, (self.context or {}).get('pid'), self.cpu
        module = self.owner(address, context)
        if not self.atui:
            self.status('angr unavailable / --atui PATH', error=True)
            return
        if not module:
            self.status('No module in this context', error=True)
            return

        def current():
            return (generation == self.generation and pid == (self.context or {}).get('pid')
                    and cpu == self.cpu and self.ready and not self.running)

        self.status('CFG / analyzing…')
        try:
            async with self.analysis_lock:
                cfg = self.cached_cfg(module, address, context, file_image)
                if cfg is None:
                    exported = await self.client.call('image' if file_image else 'export', cpu=cpu,
                                                      base=module['base'], allow_missing=True)
                    if not current(): return
                    rva = address - module['base']
                    if (rva & ~4095) in exported['missing_page_rvas']:
                        raise RuntimeError('Function page absent · select File')
                    analysis_path, _ = await asyncio.to_thread(
                        prepare_rva_image, exported,
                        allow_unrelocated=not file_image)
                    if not current(): return
                    cfg = await self.run_angr_cfg(analysis_path, rva)
                    if not current(): return
                    key = (generation, pid, cpu, context, module['base'], file_image, cfg.get('addr', rva))
                    if len(self.cfg_cache) >= 128:
                        self.cfg_cache.pop(next(iter(self.cfg_cache)))
                    self.cfg_cache[key] = cfg
            if not current(): return
            self.render_cfg(module, cfg, address, xrefs)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            if current():
                self.status(str(error), error=True)
                self.log_message(error)

    def render_cfg(self, module, cfg, address, xrefs=False):
        base = module['base']
        rva = address - base
        selected = next((block for block in cfg['blocks']
                         if block['addr'] <= rva < block['addr'] + max(1, block['size'])), None)
        selected_rva = selected['addr'] if selected else cfg.get('addr', rva)
        incoming, outgoing = {}, {}
        for edge in cfg['edges']:
            incoming[edge['dst']] = incoming.get(edge['dst'], 0) + 1
            outgoing[edge['src']] = outgoing.get(edge['src'], 0) + 1

        blocks = self.query_one('#cfg-blocks', DataTable)
        blocks.clear()
        selected_row = 0
        for row, block in enumerate(sorted(cfg['blocks'], key=lambda item: item['addr'])):
            runtime = base + block['addr']
            blocks.add_row(f'{runtime:016x}', str(block['size']), str(len(block.get('insns', ()))),
                           str(incoming.get(block['addr'], 0)), str(outgoing.get(block['addr'], 0)),
                           key=str(runtime))
            if block['addr'] == selected_rva:
                selected_row = row
        blocks.move_cursor(row=selected_row)

        edges = self.query_one('#cfg-edges', DataTable)
        edges.clear()
        visible = [edge for edge in cfg['edges'] if not xrefs or edge['dst'] == selected_rva]
        for row, edge in enumerate(sorted(visible, key=lambda item: (item['src'], item['dst'], item['kind']))):
            edges.add_row(f"{base + edge['src']:016x}", str(edge['kind']),
                          f"{base + edge['dst']:016x}", key=str(row))
        name = cfg.get('name') or f"sub_{cfg.get('addr', rva):x}"
        mode = f' · XREFS {len(visible)}' if xrefs else ''
        size = sum(block['size'] for block in cfg['blocks'])
        instructions = sum(len(block.get('insns', ())) for block in cfg['blocks'])
        unresolved = len(cfg.get('unresolved', ()))
        detail = (f"{module['name']}!{name} · {size}B · {instructions} insns · "
                  f"{len(cfg['blocks'])} blocks · {len(cfg['edges'])} edges")
        if unresolved:
            detail += f' · {unresolved} unresolved'
        self.query_one('#cfg-title', Static).update(Text(detail + mode))
        self.query_one(TabbedContent).active = 'cfg-tab'
        self.call_after_refresh((edges if xrefs and visible else blocks).focus)
        self.status(f"CFG / {module['name']}+0x{cfg.get('addr', rva):x}")

    async def run_atui(self, path, *arguments):
        process = None
        try:
            process = await asyncio.create_subprocess_exec(str(self.atui), str(path), *arguments,
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, start_new_session=True)
            stdout, stderr = await asyncio.wait_for(process.communicate(), 180)
            if process.returncode:
                raise RuntimeError(stderr.decode(errors='replace')[-4000:] or 'angr analysis failed')
            return stdout.decode(errors='replace')
        finally:
            if process and process.returncode is None:
                try: os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError: pass
                try:
                    await asyncio.wait_for(process.wait(), 5)
                except asyncio.TimeoutError:
                    try: os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError: pass
                    await process.wait()

    async def run_angr(self, path, rva):
        return await self.run_atui(path, '--dec', hex(rva))

    async def decompile(self, address, allow_missing=False, file_image=False, revision=None, context=True):
        if revision is None:
            self.analysis_mode = 'file' if file_image else 'live'
            self.query_one('#analysis-source', Button).label = self.analysis_mode.title()
            await self.show_address(address, context=context, analyze=False)
            revision = self.view_revision
        generation, pid, cpu = self.generation, (self.context or {}).get('pid'), self.cpu
        module = self.owner(address, context)
        def current():
            return (revision == self.view_revision and generation == self.generation
                    and pid == (self.context or {}).get('pid') and self.ready and not self.running)
        async with self.analysis_lock:
            if not current(): return
            try:
                if not self.atui:
                    self.query_one('#analysis-status', Static).update('angr unavailable · --atui PATH')
                    return
                if not module:
                    raise RuntimeError('No module in this context')
                source = 'FILE' if file_image else 'LIVE'
                key = (generation, pid, cpu, context, module['base'], address, allow_missing, file_image)
                if key in self.analysis_cache:
                    rendered, status, width = self.analysis_cache[key]
                else:
                    self.query_one('#analysis-status', Static).update(f'DECOMPILE · {source} · …')
                    exported = await self.client.call('image' if file_image else 'export', cpu=cpu,
                                                      base=module['base'], allow_missing=allow_missing)
                    if not current(): return
                    missing = exported['missing_page_rvas']
                    rva = address - module['base']
                    if (rva & ~4095) in missing:
                        raise RuntimeError('Function page absent · select File')
                    preparation = asyncio.create_task(asyncio.to_thread(prepare_rva_image, exported,
                                                                        allow_unrelocated=allow_missing and not file_image))
                    try:
                        analysis_path, provenance = await asyncio.shield(preparation)
                    except asyncio.CancelledError:
                        # Do not race two writers of the same cached RVA image.
                        await asyncio.gather(preparation, return_exceptions=True)
                        raise
                    if not current(): return
                    code = await self.run_angr(analysis_path, rva)
                    if not current(): return
                    if not code.strip() or code.lstrip().startswith('// no function recovered'):
                        raise RuntimeError('angr could not recover this function')
                    status = f'DECOMPILE · {source}'
                    if missing: status += f' · PARTIAL {len(missing)}p'
                    if provenance['warning']: status += ' · reloc?'
                    rendered = Group(
                        Text(f"{module['name']}+0x{rva:x} · RVA\n", style='#72d6cb'),
                        Syntax(code, 'c', theme='monokai', background_color='#101720',
                               line_numbers=True, word_wrap=False))
                    lines = code.expandtabs(4).splitlines()
                    width = max(len(line) for line in lines) + len(str(len(lines))) + 6
                    if len(self.analysis_cache) >= 128:
                        self.analysis_cache.pop(next(iter(self.analysis_cache)))
                    self.analysis_cache[key] = rendered, status, width
                if current():
                    self.query_one('#analysis-status', Static).update(status)
                    self.query_one('#decompiler', Static).styles.width = width
                    self.query_one('#decompiler', Static).update(rendered)
            except asyncio.CancelledError:
                raise
            except Exception as error:
                if current():
                    self.query_one('#analysis-status', Static).update('DECOMPILE · unavailable')
                    self.query_one('#decompiler', Static).update(Text(str(error)))


def main():
    parser = argparse.ArgumentParser(description='vroom Windows kernel debugger')
    parser.add_argument('config', type=Path)
    parser.add_argument('--backend', type=Path, default=Path(__file__).resolve().parents[3]/'target/research/debug/vroom-lab')
    parser.add_argument('--atui', type=Path, help='angr_tui executable or run-atui-macos.sh')
    parser.add_argument('--restore', type=Path, help='vroom checkpoint directory')
    parser.add_argument('--ex-callback-demo', action='store_true', help='load symbols and Ex callback arguments from a paused checkpoint')
    args = parser.parse_args()
    KernelApp(args.backend.resolve(), args.config.resolve(), atui=args.atui.resolve() if args.atui else None, restore=args.restore, ex_callback_demo=args.ex_callback_demo).run()

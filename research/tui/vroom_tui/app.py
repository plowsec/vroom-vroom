"""Windows kernel workspace: live state, modules and callback ownership."""
import argparse
import asyncio
import shlex
from pathlib import Path

from capstone import Cs, CS_ARCH_ARM64, CS_MODE_ARM
from rich.text import Text
from rich.console import Group
from rich.syntax import Syntax
from textual import on, work
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, Vertical, VerticalScroll
from textual.widgets import DataTable, Footer, Header, Input, Label, RichLog, Static, TabbedContent, TabPane

from .client import Client
from .analysis import prepare_rva_image


class KernelApp(App):
    TITLE = 'vroom / kernel'
    SUB_TITLE = 'Windows ARM64'
    CSS = """
    Screen { background: #101720; color: #d7e0eb; }
    Header { background: #182431; }
    #status { height: 2; padding: 0 2; background: #182431; color: #72d6cb; }
    #workspace { height: 1fr; }
    #sidebar { width: 38; border-right: solid #2c4054; }
    .section { height: 2; padding: 0 1; color: #7fa7ce; text-style: bold; }
    #modules { height: 1fr; background: #101720; }
    #callbacks { height: 1fr; background: #101720; }
    #center { width: 1fr; }
    #register-pane { width: 34; border-left: solid #2c4054; }
    #registers { padding: 0 1; }
    #code { padding: 1 2; }
    #process-event { height: 1fr; background: #101720; }
    #events { height: 8; border-top: solid #2c4054; background: #0c121a; padding: 0 1; }
    #command { dock: bottom; height: 3; border: solid #38516b; }
    Footer { background: #182431; }
    DataTable > .datatable--cursor { background: #234a64; color: #ffffff; }
    Screen.narrow #sidebar, Screen.narrow #register-pane { display: none; }
    Screen.narrow.show-sidebar #sidebar { display: block; width: 32; }
    Screen.narrow.show-registers #register-pane { display: block; width: 30; }
    Screen.short #events { height: 4; }
    Screen.short .section, Screen.short #status { height: 1; }
    """
    BINDINGS = [
        Binding('f4', 'discover', 'Windows'), Binding('f5', 'resume', 'Continue'),
        Binding('f6', 'pause', 'Pause'), Binding('f7', 'step', 'Step'),
        Binding('f2', 'breakpoint', 'Breakpoint'), Binding('colon', 'command', 'Command'),
        Binding('f9', 'panels', 'Panels'),
        Binding('ctrl+q', 'poweroff', 'Power off & quit'),
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
        self.generation = 0
        self.running = False
        self.ready = False
        self.decoder = Cs(CS_ARCH_ARM64, CS_MODE_ARM)
        self.atui = atui
        self.ex_callback_demo = ex_callback_demo

    def compose(self) -> ComposeResult:
        yield Header()
        yield Static('Connecting to vroom-lab…', id='status')
        with Horizontal(id='workspace'):
            with Vertical(id='sidebar'):
                yield Label('LOADED MODULES', classes='section')
                yield DataTable(id='modules', cursor_type='row')
                yield Label('PROCESS CALLBACKS', classes='section')
                yield DataTable(id='callbacks', cursor_type='row')
            with Vertical(id='center'):
                with TabbedContent():
                    with TabPane('Code', id='code-tab'):
                        with VerticalScroll():
                            yield Static('', id='code')
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
                        with VerticalScroll():
                            yield Static('', id='memory')
                    with TabPane('Decompiler', id='decompiler-tab'):
                        with VerticalScroll():
                            yield Static('', id='decompiler')
            with VerticalScroll(id='register-pane'):
                yield Label('CPU / REGISTERS', classes='section')
                yield Static('', id='registers')
        yield RichLog(id='events', wrap=True, markup=False)
        yield Input(placeholder=':', id='command')
        yield Footer()

    async def on_mount(self):
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
        if not self.ready or self.running:
            return
        try:
            if event.pane.id == 'functions-tab': self.refresh_functions()
            elif event.pane.id == 'processes-tab': await self.load_processes()
            elif event.pane.id == 'peb-tab' and (self.context or {}).get('pid'): await self.show_peb()
            elif event.pane.id == 'stack-tab': await self.show_stack()
        except Exception as error: self.log_message(error)

    def on_resize(self, event):
        self.screen.set_class(event.size.width < 120, 'narrow')
        self.screen.set_class(event.size.height < 35, 'short')

    def action_panels(self):
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

    def status(self, text):
        if text.startswith('PAUSED') and (self.context or {}).get('pid') and 'PID ' not in text:
            text += f" / PID {self.context['pid']}"
        self.query_one('#status', Static).update(text)

    def event(self, event):
        kind = event['event']
        if kind in ('ready', 'stopped', 'fault', 'reset_requested'):
            self.generation += 1
            self.analysis_cache.clear()
            self.ready = True
            self.running = False
            self.cpu = event.get('cpu', self.cpu)
            self.status(f"PAUSED  /  CPU {self.cpu}  /  {event.get('reason', kind)}")
            self.refresh_state()
            if kind == 'ready' and self.ex_callback_demo:
                self.start_demo()
        elif kind in ('shutdown', 'disconnected'):
            self.ready = False
            self.running = False
            self.status('DISCONNECTED  /  backend stopped')
        self.log_message(event)

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
        try:
            regs = await self.client.call('registers', cpu=self.cpu)
            lines = [f"PC   {regs['pc']:016x}", f"SP   {regs['sp']:016x}", f"PS   {regs['pstate']:08x}", '']
            lines += [f"x{i:<2}  {value:016x}" for i, value in enumerate(regs['x'])]
            lines += ['', f"TTBR1 {regs['ttbr1']:016x}"]
            self.query_one('#registers', Static).update('\n'.join(lines))
            await self.disassemble(regs['pc'], context=False)
        except Exception as error:
            self.log_message(error)

    async def disassemble(self, address, context=True):
        self.address = address
        data = await self.client.call('read_virtual', cpu=self.cpu, address=address, length=128, context=context)
        text = Text()
        owner = next((m for m in self.modules if m['base'] <= address < m['base'] + m['size']), None)
        title = f"{owner['name']} + 0x{address-owner['base']:x}" if owner else f'0x{address:016x}'
        text.append(title + '\n\n', style='bold #72d6cb')
        for instruction in self.decoder.disasm(bytes.fromhex(data['hex']), address):
            mark = '●' if instruction.address in self.breakpoints else ' '
            text.append(f'{mark} {instruction.address:016x}  ', style='#69839c')
            text.append(f'{instruction.mnemonic:<9}', style='#72d6cb')
            text.append(instruction.op_str + '\n', style='#d7e0eb')
        self.query_one('#code', Static).update(text)

    @work(group='control', exclusive=True)
    async def action_pause(self):
        try:
            await self.client.call('pause')
            self.running = False
            self.status(f'PAUSED  /  CPU {self.cpu}')
            self.refresh_state()
        except Exception as error: self.log_message(error)

    @work(group='control', exclusive=True)
    async def action_resume(self):
        try:
            await self.client.call('resume')
            self.running = True
            self.status('RUNNING')
        except Exception as error: self.log_message(error)

    @work(group='control', exclusive=True)
    async def action_step(self):
        try:
            await self.client.call('step', cpu=self.cpu)
            self.status(f'STEPPING  /  CPU {self.cpu}')
        except Exception as error: self.log_message(error)

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
        table = self.query_one('#modules', DataTable)
        table.clear()
        for i, module in enumerate(self.modules):
            table.add_row(module['name'], f"{module['size']//1024}K", key=str(i))

    async def load_processes(self):
        self.processes = await self.client.call('processes', cpu=self.cpu)
        table = self.query_one('#processes', DataTable)
        table.clear()
        for i, p in enumerate(self.processes):
            table.add_row(str(p['pid']), p['name'], str(p['parent_pid']), f"{p['eprocess']:016x}", key=str(i))
        self.query_one(TabbedContent).active = 'processes-tab'

    async def select_process(self, pid=None):
        try:
            self.context = await self.client.call('process', cpu=self.cpu, **({'pid':pid} if pid is not None else {}))
            await self.load_modules()
            if pid is not None:
                self.status(f"PAUSED / CPU {self.cpu} / PID {pid} / {self.context['name']}")
                await self.show_peb()
            else:
                self.status(f'PAUSED / CPU {self.cpu}')
        except Exception as error:
            self.log_message(error)

    async def show_peb(self):
        peb = await self.client.call('peb', cpu=self.cpu)
        table = self.query_one('#peb', DataTable)
        table.clear()
        table.add_rows((key, hex(value) if isinstance(value, int) else str(value or '—')) for key, value in peb.items())
        self.query_one(TabbedContent).active = 'peb-tab'

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

    async def show_stack(self):
        result = await self.client.call('stack', cpu=self.cpu)
        table = self.query_one('#stack', DataTable)
        table.clear()
        for i, f in enumerate(result['frames']):
            table.add_row(str(i), f"{f['pc']:016x}", f"{f['sp']:016x}", f.get('symbol') or '', f['method'])
        if result.get('stop'): self.log_message(result['stop'])
        self.query_one(TabbedContent).active = 'stack-tab'

    async def load_callbacks(self):
        self.callbacks = await self.client.call('callbacks', cpu=self.cpu)
        table = self.query_one('#callbacks', DataTable)
        table.clear()
        for i, callback in enumerate(self.callbacks):
            owner = callback['module'] or 'unresolved'
            offset = callback['module_offset']
            table.add_row(str(callback['slot']), f"{owner}+0x{offset:x}" if offset is not None else owner, key=str(i))
        self.log_message(f'{len(self.callbacks)} live process callbacks; select one to inspect')
        for source in sorted({c.get('layout_source', 'fixture') for c in self.callbacks}):
            self.log_message('Layout evidence: ' + source)

    @work(group='control', exclusive=True)
    async def action_breakpoint(self):
        try: await self.toggle_breakpoint(self.address)
        except Exception as error: self.log_message(error)

    async def toggle_breakpoint(self, address):
        addresses = list(self.breakpoints)
        if address in addresses: addresses.remove(address)
        else: addresses.append(address)
        self.breakpoints = await self.client.call('breakpoints', addresses=addresses)
        self.log_message('Hardware breakpoints: ' + ', '.join(f'0x{x:x}' for x in self.breakpoints))
        await self.disassemble(address)

    def action_command(self):
        self.query_one('#command', Input).focus()

    @on(DataTable.RowSelected)
    def selected(self, event):
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
        address = self.callbacks[index]['function'] if event.data_table.id == 'callbacks' else self.modules[index]['base']
        self.run_worker(self.show_address(address), group='browse', exclusive=True)

    async def show_address(self, address):
        try:
            await self.disassemble(address)
            self.query_one(TabbedContent).active = 'code-tab'
        except Exception as error: self.log_message(error)

    @on(Input.Submitted, '#command')
    @work(group='commands', exclusive=True)
    async def command(self, event):
        event.input.value = ''
        try:
            words = shlex.split(event.value.lstrip(':').strip())
            if not words: return
            op, *args = words
            if op == 'callbacks': await self.load_callbacks()
            elif op == 'event':
                await self.show_process_event()
            elif op == 'modules': await self.load_modules()
            elif op == 'ps': await self.load_processes()
            elif op == 'process': await self.select_process(int(args[0], 0) if args else None)
            elif op == 'peb': await self.show_peb()
            elif op == 'stack': await self.show_stack()
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
                self.log_message(self.watchpoints)
            elif op == 'wc':
                self.watchpoints = await self.client.call('watchpoints', watches=[])
            elif op == 'image':
                module = self.module_named(args[0])
                exported = await self.client.call('image', cpu=self.cpu, base=module['base'], **({'path':args[1]} if len(args)>1 else {}))
                self.log_message(exported['source'])
            elif op == 'cpu':
                self.cpu = int(args[0]); self.refresh_state()
            elif op == 'bc':
                self.breakpoints = await self.client.call('breakpoints', addresses=[])
                self.log_message('All hardware breakpoints cleared')
            elif op in ('dec', 'dec-file', 'dec-partial'):
                address = await self.symbol_address(args[0]) if args else self.address
                self.run_worker(self.decompile(address, allow_missing=op == 'dec-partial', file_image=op == 'dec-file'), group='decompile', exclusive=True)
            elif op in ('bp', 'dis', 'mem', 'x'):
                address = await self.symbol_address(args[0])
                if op == 'bp': await self.toggle_breakpoint(address)
                elif op == 'dis': await self.show_address(address)
                elif op == 'x': self.log_message(f'{args[0]} = 0x{address:016x}')
                else:
                    result = await self.client.call('read_virtual', address=address, cpu=self.cpu, length=int(args[1], 0) if len(args)>1 else 256)
                    data = bytes.fromhex(result['hex'])
                    self.query_one('#memory', Static).update('\n'.join(f'{address+i:016x}  {data[i:i+16].hex(" ")}' for i in range(0, len(data), 16)))
                    self.query_one(TabbedContent).active = 'memory-tab'
            elif op == 'dump': self.log_message(await self.client.call('dump', cpu=self.cpu, address=await self.symbol_address(args[0]), length=int(args[1],0)))
            elif op == 'checkpoint': self.log_message(await self.client.call('checkpoint', path=args[0]))
            else: self.log_message('ps · process PID · peb · modules · symbols MODULE · functions MODULE PREFIX · next/prev · stack · bp/dis/mem/x SYMBOL · watch ADDRESS SIZE r/w/rw · bc/wc · dec/dec-file/dec-partial SYMBOL · image MODULE [PATH] · dump ADDRESS N · callbacks · event · cpu N · checkpoint PATH')
        except Exception as error: self.log_message(error)

    @work(group='exit', exclusive=True)
    async def action_poweroff(self):
        await self.client.close()
        self.exit()

    async def on_unmount(self):
        await self.client.close()

    async def decompile(self, address, allow_missing=False, file_image=False):
        process = None
        try:
            if not self.atui:
                raise RuntimeError('Pass --atui /path/to/run-atui-macos.sh to enable angr analysis')
            module = next((m for m in self.modules if m['base'] <= address < m['base'] + m['size']), None)
            if not module:
                raise RuntimeError('Load Windows modules before decompiling')
            self.query_one(TabbedContent).active = 'decompiler-tab'
            cache_key = ('file' if file_image else self.generation, self.generation, (self.context or {}).get('pid'), module['base'], address, allow_missing)
            if cache_key in self.analysis_cache:
                self.query_one('#decompiler', Static).update(self.analysis_cache[cache_key])
                return
            self.query_one('#decompiler', Static).update('angr…')
            # Strict export: missing pages must not be mistaken for real code.
            exported = await self.client.call('image' if file_image else 'export', cpu=self.cpu, base=module['base'], allow_missing=allow_missing)
            missing = exported['missing_page_rvas']
            if ((address - module['base']) & ~4095) in missing:
                raise RuntimeError('The requested function is on a missing page')
            analysis_path, provenance = await asyncio.to_thread(prepare_rva_image, exported, allow_unrelocated=allow_missing)
            process = await asyncio.create_subprocess_exec(str(self.atui), str(analysis_path), '--dec', hex(address-module['base']),
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            stdout, stderr = await asyncio.wait_for(process.communicate(), 180)
            if process.returncode:
                raise RuntimeError(stderr.decode(errors='replace')[-4000:] or 'angr analysis failed')
            warning = f'PARTIAL · {len(missing)} missing pages\n' if missing else ''
            if provenance['warning']:
                warning += 'Relocations unavailable · absolute internal pointers unresolved\n'
            mapping = ('FILE / ' if file_image else 'LIVE / ') + f"{module['name']} / runtime 0x{address:x} / RVA 0x{address-module['base']:x}\nRVA view\n\n"
            rendered = Group(
                Text(warning, style='#e9b65d'), Text(mapping, style='#72d6cb'),
                Syntax(stdout.decode(errors='replace'), 'c', theme='monokai',
                       background_color='#101720', line_numbers=True))
            if len(self.analysis_cache) >= 128:
                self.analysis_cache.pop(next(iter(self.analysis_cache)))
            self.analysis_cache[cache_key] = rendered
            self.query_one('#decompiler', Static).update(rendered)
        except asyncio.CancelledError:
            raise
        except Exception as error:
            self.query_one('#decompiler', Static).update(Text(str(error)))
        finally:
            if process and process.returncode is None:
                process.terminate()
                try:
                    await asyncio.wait_for(process.wait(), 5)
                except asyncio.TimeoutError:
                    process.kill()
                    await process.wait()


def main():
    parser = argparse.ArgumentParser(description='vroom Windows kernel debugger')
    parser.add_argument('config', type=Path)
    parser.add_argument('--backend', type=Path, default=Path(__file__).resolve().parents[3]/'target/research/debug/vroom-lab')
    parser.add_argument('--atui', type=Path, help='angr_tui executable or run-atui-macos.sh')
    parser.add_argument('--restore', type=Path, help='vroom checkpoint directory')
    parser.add_argument('--ex-callback-demo', action='store_true', help='load symbols and Ex callback arguments from a paused checkpoint')
    args = parser.parse_args()
    KernelApp(args.backend.resolve(), args.config.resolve(), atui=args.atui.resolve() if args.atui else None, restore=args.restore, ex_callback_demo=args.ex_callback_demo).run()

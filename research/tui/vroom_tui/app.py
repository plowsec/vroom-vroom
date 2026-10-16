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
        yield Input(placeholder=': bp nt!PsSetCreateProcessNotifyRoutineEx  ·  callbacks  ·  dis ADDRESS  ·  mem ADDRESS', id='command')
        yield Footer()

    async def on_mount(self):
        self.query_one('#modules', DataTable).add_columns('Module', 'Size')
        self.query_one('#callbacks', DataTable).add_columns('Slot', 'Owner / offset')
        self.query_one('#process-event', DataTable).add_columns('Field', 'Value')
        try:
            await self.client.start()
        except (OSError, RuntimeError) as error:
            self.status(f'FAILED  /  {error}')
            self.log_message(error)

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
        self.query_one('#status', Static).update(text)

    def event(self, event):
        kind = event['event']
        if kind in ('ready', 'stopped', 'fault', 'reset_requested'):
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
            await self.disassemble(regs['pc'])
        except Exception as error:
            self.log_message(error)

    async def disassemble(self, address):
        self.address = address
        data = await self.client.call('read_virtual', cpu=self.cpu, address=address, length=128)
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
            self.status('RUNNING  /  F6 pauses all CPUs and devices')
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
        table = self.query_one('#modules', DataTable)
        table.clear()
        for i, module in enumerate(self.modules):
            table.add_row(module['name'], f"{module['size']//1024}K", key=str(i))

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
            elif op == 'cpu':
                self.cpu = int(args[0]); self.refresh_state()
            elif op == 'bc':
                self.breakpoints = await self.client.call('breakpoints', addresses=[])
                self.log_message('All hardware breakpoints cleared')
            elif op in ('dec', 'dec-partial'):
                address = int(args[0], 0) if args else self.address
                self.run_worker(self.decompile(address, allow_missing=op == 'dec-partial'), group='decompile', exclusive=True)
            elif op in ('bp', 'dis', 'mem', 'x'):
                address = int(args[0], 0) if args[0].startswith(('0x', '0X')) or args[0].isdigit() else await self.client.call('resolve', name=args[0])
                if op == 'bp': await self.toggle_breakpoint(address)
                elif op == 'dis': await self.show_address(address)
                elif op == 'x': self.log_message(f'{args[0]} = 0x{address:016x}')
                else:
                    result = await self.client.call('read_virtual', address=address, cpu=self.cpu, length=int(args[1], 0) if len(args)>1 else 256)
                    data = bytes.fromhex(result['hex'])
                    self.query_one('#memory', Static).update('\n'.join(f'{address+i:016x}  {data[i:i+16].hex(" ")}' for i in range(0, len(data), 16)))
                    self.query_one(TabbedContent).active = 'memory-tab'
            elif op == 'checkpoint': self.log_message(await self.client.call('checkpoint', path=args[0]))
            else: self.log_message('Commands: callbacks, event (Ex entry), modules, cpu N, bp SYMBOL, bc, dis ADDRESS, dec ADDRESS, dec-partial ADDRESS, mem ADDRESS [N], x SYMBOL, checkpoint PATH')
        except Exception as error: self.log_message(error)

    @work(group='exit', exclusive=True)
    async def action_poweroff(self):
        await self.client.close()
        self.exit()

    async def on_unmount(self):
        await self.client.close()

    async def decompile(self, address, allow_missing=False):
        process = None
        try:
            if not self.atui:
                raise RuntimeError('Pass --atui /path/to/run-atui-macos.sh to enable angr analysis')
            module = next((m for m in self.modules if m['base'] <= address < m['base'] + m['size']), None)
            if not module:
                raise RuntimeError('Load Windows modules before decompiling')
            self.query_one(TabbedContent).active = 'decompiler-tab'
            self.query_one('#decompiler', Static).update('Exporting loaded image and starting angr…')
            # Strict export: missing pages must not be mistaken for real code.
            exported = await self.client.call('export', cpu=self.cpu, base=module['base'], allow_missing=allow_missing)
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
            mapping = f"{module['name']} / runtime 0x{address:x} / RVA 0x{address-module['base']:x}\nCode: RVA · external pointers: runtime VA\n\n"
            self.query_one('#decompiler', Static).update(Group(
                Text(warning, style='#e9b65d'), Text(mapping, style='#72d6cb'),
                Syntax(stdout.decode(errors='replace'), 'c', theme='monokai',
                       background_color='#101720', line_numbers=True)))
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

from textual.widgets import DataTable, Input, Static, TabbedContent
from vroom_tui.app import KernelApp


class FakeClient:
    """UI-only fixture; deliberately uses example.sys, never a vendor capture."""
    def __init__(self):
        self.calls = []
        self.process = None
        self.app = None

    async def start(self):
        self.app.event({'event': 'ready'})

    async def close(self):
        pass

    async def call(self, op, **args):
        self.calls.append((op, args))
        if op == 'registers':
            return dict(cpu=0, pc=0xfffff80000001000, sp=0xffff800000004000,
                        pstate=5, ttbr1=0x1800, x=list(range(31)))
        if op == 'read_virtual':
            return dict(hex='1f2003d5c0035fd6')  # nop; ret
        if op == 'kernel':
            return dict(name='ntoskrnl.exe', base=0xfffff80000000000)
        if op == 'symbols':
            return dict(count=2)
        if op == 'modules':
            return [dict(name='example.sys', base=0xfffff80000001000, size=4096)]
        if op == 'callbacks':
            return [dict(slot=1, function=0xfffff80000001000, module='example.sys', module_offset=0)]
        if op == 'resolve':
            return 0xfffff80000001000
        if op == 'breakpoints':
            return args['addresses']
        if op == 'process_event':
            return dict(kind='create', pid=42, process=0xffff800000008000,
                        parent_pid=4, creator_pid=4, creator_tid=8,
                        image='example.exe', command_line='example.exe', creation_status=0)
        return 'ok'


async def test_pause_discover_callback_and_breakpoint():
    client = FakeClient()
    app = KernelApp(client=client)
    client.app = app
    async with app.run_test(size=(150, 45)) as pilot:
        await pilot.pause()
        await pilot.press('f4')
        await app.workers.wait_for_complete()
        assert app.query_one('#modules', DataTable).row_count == 1
        command = app.query_one('#command', Input)
        command.value = 'callbacks'
        command.focus()
        await pilot.press('enter')
        await pilot.pause()
        assert app.query_one('#callbacks', DataTable).row_count == 1
        await pilot.press('f2')
        await app.workers.wait_for_complete()
        assert app.breakpoints == [0xfffff80000001000]
        await pilot.press('f5')
        await app.workers.wait_for_complete()
        assert app.running
        await pilot.press('f6')
        await app.workers.wait_for_complete()
        assert not app.running
        assert any(op == 'pause' for op, _ in client.calls)
        assert app.query_one('#registers', DataTable).get_row_at(0)[0] == 'PC'


async def test_callback_tab_contains_live_fields():
    client = FakeClient()
    app = KernelApp(client=client, ex_callback_demo=True)
    client.app = app
    async with app.run_test(size=(150, 45)) as pilot:
        await pilot.pause()
        await app.workers.wait_for_complete()
        assert app.query_one(TabbedContent).active == 'code-tab'
        app.query_one(TabbedContent).active = 'callback-tab'
        await pilot.pause()
        fields = app.query_one('#process-event', DataTable)
        assert fields.get_row_at(1) == ['PID', '42']
        assert fields.get_row_at(6) == ['Image', 'example.exe']
        assert not app.query('#lesson')
        fields.focus()
        await pilot.press('enter')
        await pilot.pause()
        assert app.query_one(TabbedContent).active == 'callback-tab'
        command = app.query_one('#command', Input)
        command.value = 'dis nt!KeBugCheckEx'
        command.focus()
        await pilot.press('enter')
        await pilot.pause()
        assert app.query_one(TabbedContent).active == 'code-tab'


async def test_startup_failure_is_visible():
    class MissingBackend(FakeClient):
        async def start(self):
            raise RuntimeError('Backend lacks hypervisor entitlement. Run research/build.sh')
    app = KernelApp(client=MissingBackend())
    async with app.run_test() as pilot:
        await pilot.pause()
        assert 'FAILED' in str(app.query_one('#status', Static).content)
        assert 'research/build.sh' in str(app.query_one('#status', Static).content)
        assert not app.ready


async def test_narrow_terminal_preserves_code_space():
    client = FakeClient()
    app = KernelApp(client=client)
    client.app = app
    async with app.run_test(size=(80, 24)) as pilot:
        await pilot.pause()
        assert app.query_one('#center').size.width == 80
        await pilot.press('f9')
        await pilot.pause()
        assert app.query_one('#sidebar').display
        assert app.query_one('#center').size.width >= 48
        await pilot.press('f9')
        await pilot.pause()
        assert app.query_one('#register-pane').display
        assert app.query_one('#center').size.width >= 50


async def test_function_filter_process_selection_and_qualified_symbols():
    class WindowsClient(FakeClient):
        async def call(self, op, **args):
            if op == 'functions':
                self.calls.append((op, args))
                return dict(rows=[dict(name='NtCreateFile', address=0x70001000)], more=False)
            if op == 'processes':
                return [dict(pid=42, parent_pid=4, name='example.exe', eprocess=0xffff800000009000)]
            if op == 'process': return dict(pid=42, name='example.exe')
            if op == 'user_modules': return [dict(name='ntdll.dll',base=0x70000000,size=0x20000)]
            if op == 'peb': return dict(address=0x60000000, command_line='example.exe --test')
            return await super().call(op, **args)
    client = WindowsClient()
    app = KernelApp(client=client)
    client.app = app
    async with app.run_test(size=(150,45)) as pilot:
        await app.load_processes()
        assert app.query_one('#processes',DataTable).get_row_at(0)[0] == '42'
        await app.select_process(42)
        assert app.query_one(TabbedContent).active == 'peb-tab'
        assert app.query_one('#peb',DataTable).get_row_at(1)[1] == 'example.exe --test'
        await app.symbol_address('ntdll!NtCreateFile')
        assert ('resolve',dict(base=0x70000000,name='NtCreateFile')) in client.calls
        app.query_one('#function-filter',Input).value = 'NtCreate'
        await pilot.pause(.2)
        await app.workers.wait_for_complete()
        assert app.query_one('#functions',DataTable).get_row_at(0)[0] == 'NtCreateFile'
        assert any(op=='functions' and args['prefix']=='NtCreate' for op,args in client.calls)

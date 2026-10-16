"""Split-view routing, context isolation, caching and asynchronous navigation."""
import asyncio
from pathlib import Path

from textual.widgets import Button, DataTable, Static, TabbedContent
from vroom_tui.app import KernelApp
from test_app import FakeClient

KERNEL = 0xfffff80000000000
DRIVER = 0xfffff80001000000
USER = 0x70000000


class AnalysisClient(FakeClient):
    async def call(self, op, **args):
        if op in ('export', 'image'):
            self.calls.append((op, args))
            return dict(path='/fixture.exe', image=dict(base=args['base'], size=0x10000),
                        missing_page_rvas=[0x8000] if op == 'export' else [])
        if op == 'modules':
            return [dict(name='ntoskrnl.exe', base=KERNEL, size=0x10000),
                    dict(name='example.sys', base=DRIVER, size=0x10000)]
        if op == 'user_modules': return [dict(name='ntdll.dll', base=USER, size=0x10000)]
        if op == 'callbacks': return [dict(slot=0, function=DRIVER+0x1000, module='example.sys', module_offset=0x1000)]
        if op == 'process': return dict(pid=args.get('pid'), name='example.exe', directory_table=0x8000)
        if op == 'peb': return dict(address=0x123000, command_line='example.exe')
        if op == 'resolve': return args.get('base', KERNEL)+0x2000
        return await super().call(op, **args)


def setup(monkeypatch, client=None):
    monkeypatch.setattr('vroom_tui.app.prepare_rva_image',
                        lambda exported, **kw: (Path('/fixture.rva.exe'), {'warning':None}))
    client = client or AnalysisClient()
    app = KernelApp(client=client, atui=Path('/fixture-atui'))
    client.app = app
    calls = []
    async def run(path, rva):
        calls.append(rva)
        return f'int function_{rva:x}(void) {{ return 42; }}'
    app.run_angr = run
    return app, client, calls


def pseudocode(app):
    content = app.query_one('#decompiler', Static).content
    return '\n'.join(getattr(r, 'code', str(r)) for r in content.renderables) if hasattr(content, 'renderables') else str(content)


async def test_callback_kernel_and_user_functions_share_split_view(monkeypatch):
    app, client, calls = setup(monkeypatch)
    async with app.run_test(size=(180, 50)) as pilot:
        await app.workers.wait_for_complete()
        await app.load_modules()
        await app.load_callbacks()
        table = app.query_one('#callbacks', DataTable)
        table.focus()
        await pilot.press('enter')
        await app.workers.wait_for_complete()
        await pilot.pause()
        assert app.query_one(TabbedContent).active == 'code-tab'
        assert 'function_1000' in pseudocode(app)
        assert 'PARTIAL' in str(app.query_one('#analysis-status', Static).content)
        left, right = app.query_one('#disassembly-pane'), app.query_one('#decompiler-pane')
        assert left.region.right <= right.region.x
        assert left.region.y == right.region.y and left.size.width >= 40 and right.size.width >= 40
        assert 'nop' in str(app.query_one('#code', Static).content)
        before = len(calls)
        await pilot.press('f3')
        await app.workers.wait_for_complete()
        assert len(calls) == before  # same stopped-state selection reuses analysis
        await app.show_address(await app.symbol_address('nt!Example'))
        await app.workers.wait_for_complete()
        assert client.calls[-1][0] == 'export' and client.calls[-1][1]['base'] == KERNEL
        await app.select_process(42)
        app.functions = [dict(name='NtExample', address=USER+0x2000)]
        functions = app.query_one('#functions', DataTable)
        functions.add_row('NtExample', hex(USER+0x2000), key='0')
        # Exercise the same selection event emitted by the user function table.
        app.selected(DataTable.RowSelected(functions, 0, functions.ordered_rows[0].key))
        await app.workers.wait_for_complete()
        assert app.address == USER+0x2000
        assert any(op=='export' and args['base']==USER for op,args in client.calls)
        await pilot.pause()
        assert app.query_one(TabbedContent).active == 'code-tab'
        assert await pilot.click('#analysis-source')
        await pilot.pause()
        await app.workers.wait_for_complete()
        assert app.analysis_mode == 'file'
        assert any(op=='image' and args['base']==USER for op,args in client.calls)
        assert 'FILE' in str(app.query_one('#analysis-status', Static).content)
        assert 'nop' in str(app.query_one('#code', Static).content)
        await pilot.press('f10')
        await pilot.pause()
        assert app.query_one('#center').size.width == 180


async def test_stale_exports_and_resume_cannot_replace_selected_function(monkeypatch):
    started = asyncio.Event()
    class SlowClient(AnalysisClient):
        async def call(self, op, **args):
            if op == 'export' and args['base'] == DRIVER:
                started.set()
                try: await asyncio.sleep(60)
                except asyncio.CancelledError: pass  # late transport reply
            return await super().call(op, **args)
    app, client, calls = setup(monkeypatch, SlowClient())
    async with app.run_test() as pilot:
        await app.workers.wait_for_complete()
        await app.load_modules()
        await app.show_address(DRIVER+0x1000)
        await asyncio.wait_for(started.wait(), 2)
        await app.show_address(KERNEL+0x2000)
        await app.workers.wait_for_complete()
        assert calls == [0x2000]
        assert 'function_2000' in pseudocode(app) and 'function_1000' not in pseudocode(app)
        running = asyncio.Event()
        async def slow_angr(path, rva):
            running.set()
            try: await asyncio.sleep(60)
            except asyncio.CancelledError: pass
            return 'stale code'
        app.run_angr = slow_angr
        await app.show_address(KERNEL+0x3000)
        await asyncio.wait_for(running.wait(), 2)
        await app.action_resume().wait()
        await app.workers.wait_for_complete()
        assert app.running and pseudocode(app) == ''
        assert str(app.query_one('#analysis-status', Static).content) == 'RUNNING'


async def test_missing_function_page_and_failed_disassembly_do_not_show_old_code(monkeypatch):
    class MissingClient(AnalysisClient):
        async def call(self, op, **args):
            if op == 'read_virtual' and args['address'] == DRIVER+0x8000:
                raise RuntimeError('unmapped page')
            return await super().call(op, **args)
    app, client, calls = setup(monkeypatch, MissingClient())
    async with app.run_test(size=(80,24)) as pilot:
        await app.workers.wait_for_complete()
        await app.load_modules()
        await app.show_address(KERNEL+0x1000)
        await app.workers.wait_for_complete()
        await app.show_address(DRIVER+0x8000)
        await app.workers.wait_for_complete()
        assert calls == [0x1000]
        assert 'unmapped page' in str(app.query_one('#code', Static).content)
        assert 'Function page absent' in pseudocode(app)
        assert 'function_1000' not in pseudocode(app)
        await pilot.pause()
        assert app.query_one('#decompiler-pane').region.x >= app.query_one('#disassembly-pane').region.right


async def test_user_analysis_cache_and_cpu_views_are_address_space_scoped(monkeypatch):
    app, client, calls = setup(monkeypatch)
    async with app.run_test() as pilot:
        await app.workers.wait_for_complete()
        for pid in (42, 43):
            await app.select_process(pid)
            await app.show_address(USER+0x2000)
            await app.workers.wait_for_complete()
        assert calls == [0x2000, 0x2000]
        await app.show_address(USER+0x2000)
        await app.workers.wait_for_complete()
        assert calls == [0x2000, 0x2000]
        app.cpu_ttbr0 = 0x9000  # CPU is executing a different process.
        await app.show_address(USER+0x2000, context=False)
        await app.workers.wait_for_complete()
        assert 'No module in this context' in pseudocode(app)
        assert calls == [0x2000, 0x2000]


async def test_angr_recovery_failure_is_not_cached_as_pseudocode(monkeypatch):
    app, client, calls = setup(monkeypatch)
    async def failed(path, rva):
        return '// no function recovered at 0x1000\n'
    app.run_angr = failed
    async with app.run_test() as pilot:
        await app.workers.wait_for_complete()
        await app.load_modules()
        await app.show_address(KERNEL+0x1000)
        await app.workers.wait_for_complete()
        assert 'unavailable' in str(app.query_one('#analysis-status', Static).content)
        assert 'angr could not recover' in pseudocode(app)
        assert 'nop' in str(app.query_one('#code', Static).content)
        assert not app.analysis_cache


async def test_long_pseudocode_scrolls_inside_its_pane(monkeypatch):
    from textual.containers import VerticalScroll
    app, client, calls = setup(monkeypatch)
    async def long_code(path, rva):
        return 'int f(void) { return ' + '1 + ' * 80 + '0; }'
    app.run_angr = long_code
    async with app.run_test(size=(100,32)) as pilot:
        await app.workers.wait_for_complete()
        await app.load_modules()
        await app.show_address(KERNEL+0x1000)
        await app.workers.wait_for_complete()
        await pilot.pause()
        pane = app.query_one('#decompiler-scroll', VerticalScroll)
        assert pane.max_scroll_x > 200
        pane.scroll_to(x=100, animate=False)
        await pilot.pause()
        assert pane.scroll_x == 100
        assert app.query_one('#code-scroll').scroll_x == 0
        await pilot.press('f9', 'f10')
        await pilot.pause()
        assert app.query_one('#center').size.width == 100

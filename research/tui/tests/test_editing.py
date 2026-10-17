"""Editing, execution controls and navigation on the public UI fixtures."""
import asyncio
from types import SimpleNamespace
from textual.widgets import DataTable, Input, Static, TabbedContent
from vroom_tui.app import KernelApp
from test_app import FakeClient


class EditableClient(FakeClient):
    def __init__(self):
        super().__init__()
        self.changed = 0
    async def call(self, op, **args):
        if op == 'write_register':
            self.calls.append((op,args))
            self.changed = args['value']
            return await self.call('registers')
        if op == 'registers':
            regs = await super().call(op,**args)
            regs['x'][0] = self.changed
            return regs
        if op == 'functions': return dict(rows=[],more=False)
        return await super().call(op,**args)


async def command(app, pilot, text):
    app.edit_command(text)
    await pilot.press('enter')
    await pilot.pause()
    await app.workers.wait_for_complete()


async def test_register_edit_memory_ascii_and_history():
    client=EditableClient();app=KernelApp(client=client);client.app=app
    async with app.run_test(size=(180,50)) as pilot:
        await app.workers.wait_for_complete()
        table=app.query_one('#registers',DataTable)
        table.move_cursor(row=3);table.focus()
        await pilot.press('enter');await pilot.pause()
        assert app.query_one('#command',Input).value == 'reg x0 0x0'
        await command(app,pilot,'reg x0 0x1234')
        assert client.changed == 0x1234
        value=table.get_row('x0')[1]
        assert value.plain == '0000000000001234' and 'e9b65d' in str(value.style)
        await command(app,pilot,'mem 0x1000 16')
        memory=app.query_one('#memory-table',DataTable)
        assert len(memory.get_row_at(0)[2])==8
        memory.focus();await pilot.press('enter');await pilot.pause()
        assert app.query_one('#command',Input).value.startswith('write 0x1000 ')
        await command(app,pilot,'write 0x1000 41424300')
        assert ('write_virtual',dict(cpu=0,address=0x1000,hex='41424300')) in client.calls
        await pilot.press('up');await pilot.pause()
        assert app.query_one('#command',Input).value=='write 0x1000 41424300'
        await pilot.press('up');await pilot.pause()
        assert app.query_one('#command',Input).value=='mem 0x1000 16'
        await pilot.press('down','down');await pilot.pause()
        assert app.query_one('#command',Input).value==''


async def test_step_controls_stop_race_and_breakpoint_removal():
    class StopsImmediately(EditableClient):
        async def call(self,op,**args):
            if op in ('step_over','step_out'):
                self.calls.append((op,args))
                self.app.event(dict(event='stopped',cpu=0,reason='StepOver'))
                return 'running'  # the stop event may arrive before its reply
            return await super().call(op,**args)
    client=StopsImmediately();app=KernelApp(client=client);client.app=app
    async with app.run_test(size=(180,50)) as pilot:
        await app.workers.wait_for_complete()
        assert await pilot.click('#over-button')
        await app.workers.wait_for_complete();await pilot.pause()
        assert not app.running and not app.query_one('#over-button').disabled
        await pilot.press('shift+f8');await app.workers.wait_for_complete()
        assert any(op=='step_out' for op,_ in client.calls)
        app.breakpoints=[0x1000,0x2000];app.render_breakpoints()
        await command(app,pilot,'bc 0x1000')
        assert app.breakpoints==[0x2000]
        assert app.query_one('#breakpoints',DataTable).row_count==1


async def test_module_filter_keeps_original_selection_keys():
    client=EditableClient();app=KernelApp(client=client);client.app=app
    async with app.run_test(size=(180,50)) as pilot:
        await app.workers.wait_for_complete()
        app.modules=[dict(name='alpha.sys',base=0x1000,size=4096),dict(name='beta.sys',base=0x2000,size=4096)]
        app.query_one('#module-filter',Input).value='beta'
        await pilot.pause()
        table=app.query_one('#modules',DataTable)
        assert table.row_count==1 and table.ordered_rows[0].key.value=='1'
        table.focus();await pilot.press('enter');await app.workers.wait_for_complete()
        assert app.function_base==0x2000
        assert ('symbols',dict(cpu=0,base=0x2000)) in client.calls
        assert app.query_one(TabbedContent).active=='functions-tab'


async def test_run_cannot_race_an_inflight_edit():
    started, release = asyncio.Event(), asyncio.Event()
    class SlowEdit(EditableClient):
        async def call(self, op, **args):
            if op == 'write_register':
                started.set()
                await release.wait()
            return await super().call(op, **args)
    client=SlowEdit();app=KernelApp(client=client);client.app=app
    async with app.run_test(size=(180,50)):
        await app.workers.wait_for_complete()
        edit=asyncio.create_task(app.submit_command(SimpleNamespace(value='reg x0 42')))
        try:
            await asyncio.wait_for(started.wait(), 2)
            await app.execute('resume')
            assert not any(op=='resume' for op,_ in client.calls)
            assert not app.running
        finally:
            release.set()
            await edit
        await app.execute('resume')
        assert app.running
        assert any(op=='resume' for op,_ in client.calls)

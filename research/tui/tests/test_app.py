import asyncio
import time

import pytest

from fake_lab import PROCESS, LabServer

pytestmark = pytest.mark.asyncio


def wait_for(predicate, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.01)
    return predicate()


async def settle(app, pilot):
    await app.workers.wait_for_complete()
    await pilot.pause()


@pytest.fixture
def server(app):
    server = LabServer()
    # Stopped in code the static project has, so panes navigate into it.
    server.lab.pc[0] = app.session.resolve('leaf')
    yield server
    server.close()


def pane(app, pane_id):
    return next(widget for spec, _name, widget in app._target_panes.values() if spec.pane_id == pane_id)


async def table_rows(app, pilot, widget, count):
    await asyncio.to_thread(wait_for, lambda: widget.table.row_count == count)
    await settle(app, pilot)
    return [widget.table.get_row_at(i) for i in range(widget.table.row_count)]


async def test_the_plugin_attaches_the_lab_with_its_panes(app, server):
    assert 'vroom' in app.session.plugins.target_providers()
    leaf = app.session.resolve('leaf')
    async with app.run_test(size=(180, 50)) as pilot:
        app.run_command(f'attach vroom {server.path}')
        await settle(app, pilot)
        assert app.target.name == 'vroom' and app.target.snapshot().pc == leaf
        assert {spec.pane_id for spec, _n, _w in app._target_panes.values()} >= {
            'vroom-modules', 'vroom-processes', 'vroom-stack', 'vroom-callbacks', 'vroom-snapshots'}

        await pilot.press('f3')
        await settle(app, pilot)
        modules = await table_rows(app, pilot, pane(app, 'vroom-modules'), 2)
        assert [row[0] for row in modules] == ['ntoskrnl.exe', 'driver.sys']

        app.run_command('pane vroom-callbacks')
        await settle(app, pilot)
        callbacks = pane(app, 'vroom-callbacks')
        rows = await table_rows(app, pilot, callbacks, 2)
        assert rows[0][0] == '>' and rows[1][2] == 'unresolved'
        callbacks.table.focus()
        callbacks.table.move_cursor(row=0)
        await pilot.press('enter')
        await settle(app, pilot)
        assert app.current_func is not None and app.current_func.addr == leaf

        await pilot.press('f4')
        await settle(app, pilot)
        processes = pane(app, 'vroom-processes')
        rows = await table_rows(app, pilot, processes, 3)
        assert rows[0][0] == '*' and rows[2][2] == 'app.exe'
        processes.table.focus()
        processes.table.move_cursor(row=2)
        await pilot.press('enter')
        await asyncio.to_thread(wait_for, lambda: (app.target.process or {}).get('pid') == PROCESS['pid'])
        await asyncio.to_thread(wait_for, lambda: processes.table.row_count == 3 and processes.table.get_row_at(2)[0] == '*')
        await settle(app, pilot)
        assert processes.table.get_row_at(0)[0] == ''

        app.run_command('pane vroom-snapshots')
        await settle(app, pilot)
        snapshots = pane(app, 'vroom-snapshots')
        await table_rows(app, pilot, snapshots, 1)
        snapshots.table.focus()
        await pilot.press('s')
        await asyncio.to_thread(wait_for, lambda: snapshots.table.row_count == 2)
        assert len(server.lab.snapshots) == 2

        app.run_command('vroom.cpu cpu=1')
        await settle(app, pilot)
        assert app.target.cpu == 1

        app.run_command('detach vroom')
        await settle(app, pilot)
        assert app._target_panes == {}

"""angr-style address history and on-demand CFG browsing."""
import json

import pytest
from textual.widgets import DataTable, Static, TabbedContent

from test_code_view import KERNEL, setup


CFG = {
    'addr': 0x1000,
    'name': 'ExampleFunction',
    'blocks': [
        {'addr': 0x1000, 'size': 8,
         'insns': [{'addr': 0x1000, 'text': 'b 0x1008'},
                   {'addr': 0x1004, 'text': 'nop'}]},
        {'addr': 0x1008, 'size': 8,
         'insns': [{'addr': 0x1008, 'text': 'nop'},
                   {'addr': 0x100c, 'text': 'ret'}]},
    ],
    'edges': [{'src': 0x1000, 'dst': 0x1008, 'kind': 'transition'}],
    'unresolved': [],
}


async def test_address_history_moves_back_forward_and_forks(monkeypatch):
    app, _client, _calls = setup(monkeypatch)
    async with app.run_test() as pilot:
        await app.workers.wait_for_complete()
        await app.load_modules()
        await app.show_address(KERNEL + 0x2000, analyze=False)
        await app.show_address(KERNEL + 0x3000, analyze=False)

        app.action_navigation_back()
        await pilot.pause()
        assert app.address == KERNEL + 0x2000
        app.action_navigation_forward()
        await pilot.pause()
        assert app.address == KERNEL + 0x3000

        app.action_navigation_back()
        await pilot.pause()
        await app.show_address(KERNEL + 0x4000, analyze=False)
        app.action_navigation_forward()
        await pilot.pause()
        assert app.address == KERNEL + 0x4000
        assert app.navigation_history[-1] == (KERNEL + 0x4000, True)


async def test_cfg_rebases_blocks_filters_xrefs_and_reuses_cache(monkeypatch):
    app, _client, _calls = setup(monkeypatch)
    cfg_calls = []

    async def run_cfg(path, rva):
        cfg_calls.append((path, rva))
        return CFG

    app.run_angr_cfg = run_cfg
    async with app.run_test() as pilot:
        await app.workers.wait_for_complete()
        await app.load_modules()
        await app.show_cfg(KERNEL + 0x100c)

        assert app.query_one(TabbedContent).active == 'cfg-tab'
        assert 'ExampleFunction' in str(app.query_one('#cfg-title', Static).content)
        blocks = app.query_one('#cfg-blocks', DataTable)
        assert blocks.row_count == 2 and blocks.cursor_row == 1
        assert blocks.get_row_at(0)[0] == f'{KERNEL + 0x1000:016x}'
        assert blocks.get_row_at(1)[3:] == ['1', '0']
        assert app.query_one('#cfg-edges', DataTable).row_count == 1

        await app.show_cfg(KERNEL + 0x1008, xrefs=True)
        await pilot.pause()
        assert len(cfg_calls) == 1
        edges = app.query_one('#cfg-edges', DataTable)
        assert edges.row_count == 1 and edges.has_focus
        assert 'XREFS 1' in str(app.query_one('#cfg-title', Static).content)

        edges.move_cursor(row=0)
        edges.focus()
        await pilot.press('enter')
        await app.workers.wait_for_complete()
        assert app.address == KERNEL + 0x1008


async def test_cfg_json_is_fail_closed(monkeypatch):
    app, _client, _calls = setup(monkeypatch)
    async def incomplete(*_args):
        return '{"blocks": []}'
    app.run_atui = incomplete
    with pytest.raises(RuntimeError, match='incomplete CFG'):
        await app.run_angr_cfg('/fixture', 0x1000)


async def test_cfg_batch_mode_uses_machine_readable_angr_output(monkeypatch):
    app, _client, _calls = setup(monkeypatch)
    arguments = []

    async def run_atui(path, *args):
        arguments.append((path, args))
        return json.dumps(CFG)

    app.run_atui = run_atui
    assert await app.run_angr_cfg('/fixture', 0x1000) == CFG
    assert arguments == [('/fixture', ('--pd', '0x1000', '--json'))]
    assert {'u', 'shift+u', 'x', 'ctrl+g'} <= {binding.key for binding in app.BINDINGS}

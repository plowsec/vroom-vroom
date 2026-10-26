import time

import pytest

from atui.services.commands import REGISTRY, CommandError, Dispatcher
from atui.target import BREAKPOINTS, CONTINUE, RUN_TO, SNAPSHOTS, STEP, STEP_OUT, STEP_OVER, WATCHPOINTS

from fake_lab import DRIVER, KERNEL, PROCESS, USER_DLL, LabServer
from vroom_atui.target import VroomTarget, open_vroom, parse_spec, registers


def wait_for(predicate, timeout=5.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return True
        time.sleep(0.01)
    return predicate()


@pytest.fixture
def server():
    server = LabServer()
    yield server
    server.close()


@pytest.fixture
def lab(server):
    target = open_vroom(server.path)
    stops = []
    target.subscribe(stops.append)
    target.stops = stops
    yield target
    target.detach()


def test_registers_use_atui_names():
    answer = {'cpu': 0, 'x': list(range(31)), 'pc': 8, 'sp': 16, 'pstate': 0x60000000, 'ttbr0': 1, 'ttbr1': 2,
              'tcr': 3, 'sctlr': 4, 'vbar': 5, 'sve_vl': None}
    regs = registers(answer)
    assert regs['x29'] == 29 and regs['pc'] == 8 and regs['sp'] == 16
    assert regs['flags'] == 0x60000000 and regs['ttbr0'] == 1 and 'sve_vl' not in regs


def test_spec_options():
    assert parse_spec('lab.toml,restore=snap-1,keep') == ('lab.toml', {'restore': 'snap-1', 'keep': True})
    with pytest.raises(ValueError, match='unknown option'):
        parse_spec('lab.sock,later')
    with pytest.raises(ValueError, match='lab config'):
        open_vroom('/tmp/x.sock,restore=snap-1')


def test_attached_state_and_capabilities(lab):
    assert {STEP, STEP_OVER, STEP_OUT, CONTINUE, BREAKPOINTS, WATCHPOINTS, RUN_TO, SNAPSHOTS} <= lab.capabilities
    state = lab.snapshot()
    assert state.pc == KERNEL + 0x1000 and state.regs['x1'] == 1
    assert state.stack[:2] == bytes([0x00, 0x01])


def test_steps_and_resume_stop_through_events_and_follow_the_cpu(lab, server):
    assert lab.step() is None
    assert wait_for(lambda: lab.stops)
    assert (lab.stops[-1].reason, lab.stops[-1].state.pc) == ('step', KERNEL + 0x1004)
    assert ('vm.step', {'cpu': 0}) in server.lab.calls

    lab.select_cpu(1)
    lab.add_breakpoint(KERNEL + 0x2010)
    assert server.lab.breakpoints == [KERNEL + 0x2010]
    assert lab.resume() is None
    assert wait_for(lambda: lab.stops[-1].reason == 'breakpoint')
    assert lab.stops[-1].state.pc == KERNEL + 0x2010 and lab.cpu == 1
    assert lab.stops[-1].state.regs['x0'] == 100

    count = len(lab.stops)
    stop = lab.pause()
    assert stop.reason == 'pause'
    time.sleep(0.3)
    assert len(lab.stops) == count + 1, 'a pause reports no event: one stop'


def test_inspection_refuses_while_running(lab, server):
    lab.resume()
    with pytest.raises(CommandError) as raised:
        lab.read_memory(KERNEL, 4)
    assert raised.value.code == 'target_running'
    with pytest.raises(CommandError):
        lab.modules()
    lab.pause()


def test_memory_moves_in_lab_sized_pieces(lab, server):
    data = lab.read_memory(KERNEL, 70000)
    assert len(data) == 70000 and data[0x10001] == 0x01
    reads = [args['length'] for name, args in server.lab.calls if name == 'memory.read_virtual']
    assert reads[-2:] == [65536, 70000 - 65536]
    lab.write_memory(KERNEL, bytes(20000))
    writes = [len(args['hex']) // 2 for name, args in server.lab.calls if name == 'memory.write_virtual']
    assert writes == [16384, 20000 - 16384]
    lab.write_register('x3', 0x99)
    assert lab.registers()['x3'] == 0x99


def test_modules_load_symbols_once_and_follow_the_process(lab, server):
    names = [module.name for module in lab.modules()]
    assert names == ['ntoskrnl.exe', 'driver.sys']
    lab.modules()
    assert [name for name, _ in server.lab.calls].count('kernel.symbols') == 1

    lab.select_process(PROCESS['pid'])
    assert lab.process['name'] == 'app.exe'
    assert [module.base for module in lab.modules()] == [KERNEL, DRIVER, USER_DLL]
    assert lab.stops[-1].reason == 'refresh'

    lab.select_process(None)
    assert lab.process is None and len(lab.modules()) == 2


def test_module_images_are_rva_copies(lab):
    driver = next(module for module in lab.modules() if module.base == DRIVER)
    path, file_base = driver.image()
    assert file_base == 0 and path.name.endswith('.rva.exe')
    assert path.read_bytes()[0xb0:0xb8] == bytes(8), 'image base 0'
    assert driver.image() == (path, 0)


def test_module_lists_outlive_steps(lab, server):
    def fetches():
        return [name for name, _ in server.lab.calls].count('kernel.modules')

    lab.modules()
    lab.step()
    assert wait_for(lambda: lab.stops and lab.stops[-1].reason == 'step')
    lab.modules()
    assert fetches() == 1
    lab.resume()
    lab.pause()
    lab.modules()
    assert fetches() == 2


def test_a_module_is_the_same_module_at_another_base(tmp_path):
    from atui.target.modules import identify_image

    from fake_lab import pe_image
    from vroom_atui.analysis import prepare_rva_image

    identities = []
    for n, base in enumerate((0xfffff80010000000, 0xfffff80023450000)):
        exported = tmp_path / f'export-{n}.exe'
        exported.write_bytes(pe_image(base))
        path, _ = prepare_rva_image({'path': str(exported), 'missing_page_rvas': [],
                                     'image': {'base': base, 'size': 0x3000, 'relocation_directory': [0x2000, 12]}})
        identities.append(identify_image(path, 'driver.sys', 0x3000))
    assert identities[0] == identities[1] and identities[0].digest


def test_restore_clears_the_process_and_re_arms(lab, server):
    lab.select_process(PROCESS['pid'])
    lab.add_breakpoint(KERNEL + 0x40)
    stop = lab.restore_snapshot('snap-1')
    assert lab.process is None and lab.boot == 1
    assert server.lab.breakpoints == [KERNEL + 0x40]
    assert stop.message == 'restored snap-1'
    lab.modules()
    assert [name for name, _ in server.lab.calls].count('kernel.symbols') == 2


def test_lab_commands_are_proxied_beside_the_target_s_own(session_free_dispatcher, lab):
    for spec in lab.commands():
        REGISTRY.register(spec, owner='target:vroom')
    try:
        dispatcher = session_free_dispatcher
        assert dispatcher.run('vroom.vm.status').value['cpus'] == 2
        assert dispatcher.run('vroom.cpu', {'cpu': 1}).value == {'cpu': 1}
        assert not dispatcher.run('vroom.cpu', {'cpu': 5}).ok
        assert dispatcher.run('vroom.discover').value['symbols'] == 1234
    finally:
        for spec in REGISTRY.owned_by('target:vroom'):
            REGISTRY.unregister(spec.id)


def test_a_stop_option_shuts_the_lab_down_on_detach(server):
    target = open_vroom(f'{server.path},stop')
    target.detach()
    assert ('vm.shutdown', {}) in server.lab.calls


def test_a_shutdown_is_a_lost_target(lab, server):
    server.lab.event({'event': 'shutdown'})
    assert wait_for(lambda: not lab.connected)
    assert lab.stops[-1].reason == 'finished'


def test_coverage_blocks_navigate(lab, tmp_path):
    import json

    (tmp_path / 'coverage.json').write_text(json.dumps({'executions': 3, 'new_edges': 1, 'blocks': {
        '1': {'hits': 2, 'first_input': 'aa'}, '2': {'hits': 0}}}))
    (tmp_path / 'blocks.json').write_text(json.dumps([
        {'id': 1, 'gpa': 4096, 'va': DRIVER + 0x40, 'size': 8, 'module': 'driver.sys', 'function': 'dispatch'},
        {'id': 2, 'gpa': 8192, 'va': DRIVER + 0x20, 'size': 4, 'module': 'driver.sys', 'function': 'dispatch'}]))
    summary = lab._load_coverage(None, str(tmp_path / 'coverage.json'))
    assert summary['covered_blocks'] == 1
    rows = lab._coverage_blocks(None, 'driver.sys!dispatch')
    assert [row['address'] for row in rows] == [DRIVER + 0x20, DRIVER + 0x40]
    assert rows[1]['hits'] == 2

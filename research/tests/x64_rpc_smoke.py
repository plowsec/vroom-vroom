#!/usr/bin/env python3
"""Opt-in live Windows x64 RPC test; pass the private lab socket.

Pauses the guest, reads its kernel, tests a hardware breakpoint and one step,
then clears debugger slots and leaves the VM paused. Does not patch Windows RAM.
Requires Windows to have booted, plus symbol-server access or a populated cache.
"""
import argparse
import json
import socket
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument('socket')
args = p.parse_args()
s = socket.socket(socket.AF_UNIX)
s.settimeout(180)
s.connect(args.socket)
f = s.makefile('rw')
seq = 0

def call(method, params=None, error=False):
    global seq
    seq += 1
    f.write(json.dumps(dict(jsonrpc='2.0', id=seq, method=method, params=params or {})) + '\n')
    f.flush()
    line = f.readline()
    assert line, 'server disconnected'
    reply = json.loads(line)
    assert reply['id'] == seq
    if error:
        assert 'error' in reply, reply
        return reply['error']
    assert 'error' not in reply, reply
    return reply['result']

call('initialize', {'actor': 'x64-rpc-smoke'})
names = {c['name'] for c in call('commands.list')}
assert 'cpu.registers' in names and 'kernel.processes' in names
assert 'vm.restart' in names
assert {'vm.snapshot', 'vm.snapshots', 'vm.restore'} <= names
assert 'fuzz.check' not in names
call('fuzz.check', {'harness': 'unsupported.json'}, error=True)
call('commands.run', {'command': 'fuzz.check', 'args': {'harness': 'unsupported.json'}}, error=True)
call('jobs.start', {'command': 'fuzz.check', 'args': {'harness': 'unsupported.json'}}, error=True)
call('vm.pause')
assert call('vm.status')['architecture'] == 'x86_64'
kernel = call('kernel.info')
base = hex(kernel['base'])
header = call('memory.read_virtual', {'address': base, 'length': 64})['hex']
assert header.startswith('4d5a')
translation = call('memory.translate', {'address': base})
assert call('memory.read_physical', {'address': hex(translation['physical']), 'length': 64})['hex'] == header
call('memory.read_virtual', {'address': '0x800000000000', 'length': 1}, error=True)
regs = call('cpu.registers')
call('cpu.write_register', {'register': 'rax', 'value': hex(regs['regs']['rax'])})
assert call('cpu.registers')['regs']['rax'] == regs['regs']['rax']
call('kernel.symbols')
processes = call('kernel.processes')
if isinstance(processes, dict):
    processes = processes['items']
assert any(p['pid'] == 4 and p['name'] == 'System' for p in processes)
modules = call('kernel.modules')
if isinstance(modules, dict):
    modules = modules['items']
falcon = [m for m in modules if m['name'].lower() == 'csagent.sys']
assert falcon, 'expected the enrolled Falcon kernel driver'
call('kernel.process', {'pid': 4})
assert call('memory.read_virtual', {'address': base, 'length': 2})['hex'] == '4d5a'
call('kernel.process')
address = call('kernel.resolve', {'name': 'KeQueryPerformanceCounter'})['address']
after = call('events.poll')['latest']
try:
    call('debug.breakpoints', {'addresses': [hex(address)]})
    call('vm.resume')
    events = call('events.poll', {'after': after, 'wait_ms': 10000})
    stopped = [e for e in events['events'] if e.get('reason') == 'Breakpoint']
    assert stopped and stopped[0]['pc'] == address, events
    time.sleep(.1)  # permit simultaneous vCPU stops to reach the main loop
    after = call('events.poll')['latest']
    cpu = stopped[0]['cpu']
    call('vm.step', {'cpu': cpu})
    events = call('events.poll', {'after': after, 'wait_ms': 10000})
    assert any(e.get('reason') == 'SingleStep' and e['cpu'] == cpu for e in events['events']), events
    call('debug.breakpoints', {'addresses': []})
    call('vm.resume')
    time.sleep(.3)
    assert call('vm.status')['state'] == 'Running', 'stale debug event or repeated breakpoint after clearing slots'
finally:
    call('vm.pause')
    call('debug.breakpoints', {'addresses': []})
    call('debug.watchpoints', {'watches': []})
print(json.dumps({'ok': True, 'kernel': kernel['name'], 'kernel_base': base,
                  'falcon_driver': falcon[0]['name'], 'processes': len(processes),
                  'modules': len(modules), 'state': 'Paused'}))

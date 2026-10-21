"""Diskless tracepoint integration test (HVF or KVM). Run after research/build.sh."""
import json
import queue
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path


def run(binary):
    with tempfile.TemporaryDirectory(prefix='vroom-trace-') as directory:
        root = Path(directory)
        base = 0x40000000
        image = bytearray(4096)
        struct.pack_into('<I', image, 0, 0x14000040)  # b +0x100
        struct.pack_into('<Q', image, 16, len(image))
        struct.pack_into('<I', image, 56, 0x644d5241)
        # mov x0,#0; x1 = base+0x800; loop: add x0,x0,#1; str x0,[x1]; bl +0x2c; b loop
        struct.pack_into('<7I', image, 0x100, 0xd2800000, 0xd2810001, 0xf2a80001, 0x91000400, 0xf9000020, 0x9400000b, 0x17fffffd)
        struct.pack_into('<II', image, 0x140, 0xd503201f, 0xd65f03c0)  # nop; ret
        # x0 = 0; x1 = base+0x900; x5 = base+0x240; loop: add x0,x0,#1; str x0,[x1];
        # ldr w3,[x5]; str w3,[x1,#16]; ldp x6,x7,[x5]; bl +0x18; b loop; at +0x240: nop; ret
        struct.pack_into('<12I', image, 0x200, 0xd2800000, 0xd2812001, 0xf2a80001, 0xd2804805, 0xf2a80005,
                         0x91000400, 0xf9000020, 0xb94000a3, 0xb9001023, 0xa9401ca6, 0x94000006, 0x17fffffa)
        struct.pack_into('<II', image, 0x240, 0xd503201f, 0xd65f03c0)
        (root/'Image').write_bytes(image)
        (root/'lab.toml').write_text(f'name = "trace-smoke"\ncpus = 1\nmemory_mib = 64\n[display]\ngpu = "virtio2d"\nwidth = 640\nheight = 480\n[boot]\ntype = "kernel"\nkernel = "{root / "Image"}"\n')
        with (root/'stderr.log').open('w+') as errors:
            process = subprocess.Popen([str(binary), str(root/'lab.toml')], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, text=True)
            incoming = queue.Queue()
            def reader():
                for line in process.stdout:
                    incoming.put(json.loads(line))
                incoming.put({'event':'eof'})
            threading.Thread(target=reader, daemon=True).start()
            sequence = 0
            events = []
            def get():
                try: return incoming.get(timeout=10)
                except queue.Empty:
                    errors.seek(0)
                    raise AssertionError(errors.read() or 'backend timeout')
            def call(op, **args):
                nonlocal sequence
                sequence += 1
                process.stdin.write(json.dumps(dict(id=sequence,op=op,**args))+'\n');process.stdin.flush()
                while True:
                    reply = get()
                    if 'event' in reply:
                        assert reply['event'] != 'eof', 'backend exited'
                        events.append(reply)
                    else:
                        assert reply['id'] == sequence, reply
                        if 'error' in reply: raise RuntimeError(reply['error'])
                        return reply['result']
            def refused(op, text, **args):
                try: call(op, **args)
                except RuntimeError as error: assert text in str(error), error
                else: raise AssertionError(f'{op} {args} accepted')
            def stop(reason):
                while True:
                    event = events.pop(0) if events else get()
                    if event.get('event') == 'stopped':
                        assert event['reason'] == reason, event
                        return event
                    assert event.get('event') not in ('eof','fault'),event
            try:
                first=get()
                if first.get('event') != 'ready':
                    errors.seek(0)
                    raise AssertionError(errors.read() or str(first))
                func = base+0x140
                set_ = call('tracepoints', tracepoints=[{'address': func, 'register': 'x1', 'length': 8}], capacity=100000)
                assert set_['slots'] == 1 and set_['capacity'] == 100000, set_
                call('resume')
                time.sleep(2)
                assert call('status')['state'] == 'Running'
                peek = call('traces', limit=3)
                assert len(peek['records']) == 3 and peek['remaining'] >= 3, peek
                call('pause')
                assert not [e for e in events if e.get('event') == 'stopped'], events
                drained = call('traces', clear=True)
                records = drained['records']
                assert records[:3] == peek['records'] and drained['remaining'] == 0
                for r in records:
                    assert r['pc'] == func and r['pointer'] == base+0x800 and r['x'][1] == base+0x800, r
                    assert int.from_bytes(bytes.fromhex(r['hex']), 'little') == r['x'][0], r
                counts = [r['x'][0] for r in records]
                assert counts == list(range(counts[0], counts[0]+len(counts))), 'a hit was missed'
                assert [r['seq'] for r in records] == list(range(1, len(records)+1))
                print(f'tracing: {len(records)} hits in 2 s without a stop, none missed, memory matches x0')
                # Bounded buffer, max_hits, then stop.
                call('write_register', register='pc', value=base+0x10c)
                call('tracepoints', tracepoints=[{'address': func, 'max_hits': 5}, {'address': base+0x118, 'register': 'x1', 'length': 8, 'max_hits': 3, 'stop': True}], capacity=4)
                call('resume')
                hit = stop('Tracepoint')
                assert hit['pc'] == base+0x118
                t = call('traces')
                assert [p['hits'] for p in t['tracepoints']] == [1, 1], t
                call('resume'); stop('Tracepoint'); call('resume'); stop('Tracepoint')
                call('resume'); time.sleep(0.5); call('pause')
                t = call('traces')
                assert [p['hits'] for p in t['tracepoints']] == [5, 3] and not any(p['armed'] for p in t['tracepoints']), t
                assert len(t['records']) == 4 and t['recorded'] == len(records)+8 and t['dropped'] == 4, t
                print('max_hits disarms; stop: true stops like a breakpoint; ring buffer drops the oldest')
                # Actions: skip_if + set_register, write_memory, a PC move onto another tracepoint, a failure that continues.
                call('traces', clear=True)
                call('write_register', register='pc', value=base+0x100)
                call('tracepoints', capacity=100, tracepoints=[
                    {'address': base+0x10c, 'max_hits': 6, 'actions': [
                        {'skip_if': {'register': 'w0', 'equals': 3}, 'set_register': 'x0', 'value': 100},
                        {'write_memory': {'register': 'x1', 'offset': 8, 'hex': 'efbeadde'}}]},
                    {'address': func, 'register': 'x1', 'length': 12, 'max_hits': 6, 'actions': [{'add_register': 'pc', 'value': 4}]},
                    {'address': func+4, 'max_hits': 6, 'actions': [{'write_memory': {'register': 'x0', 'hex': '00'}}, {'set_register': 'x2', 'value': 1}]}])
                call('resume'); time.sleep(0.5)
                assert call('status')['state'] == 'Running'
                call('pause')
                assert not [e for e in events if e.get('event') == 'stopped'], events
                records = call('traces', clear=True)['records']
                assert [r['tracepoint'] for r in records] == [0, 1, 2]*6, records
                counted, called, returned = records[0::3], records[1::3], records[2::3]
                assert [r['x'][0] for r in counted] == [0, 1, 2, 3, 101, 102], counted
                assert [r['actions_applied'] for r in counted] == [1, 1, 1, 2, 1, 1] and not any('action_error' in r for r in counted), counted
                for r in called:
                    assert r['actions_applied'] == 1 and r['hex'] == r['x'][0].to_bytes(8, 'little').hex()+'efbeadde', r
                assert [r['x'][0] for r in called] == [1, 2, 3, 101, 102, 103], called
                for r in returned:
                    assert r['pc'] == func+4 and r['actions_applied'] == 0 and 'action 0: write' in r['action_error'] and r['x'][2] != 1, r
                print('actions: skip_if, set/add register, write_memory, PC moved onto a tracepoint, failures recorded without a stop')
                call('tracepoints', tracepoints=[{'address': func+4, 'actions': [{'write_memory': {'register': 'x0', 'hex': '00'}, 'stop': True}]}])
                call('resume'); hit = stop('Tracepoint')
                assert hit['pc'] == func+4, hit
                assert 'action_error' in call('traces', clear=True)['records'][-1]
                refused('tracepoints', 'unknown key set_regster', tracepoints=[{'address': func, 'actions': [{'set_regster': 'x0', 'value': 0}]}])
                refused('tracepoints', 'register w31', tracepoints=[{'address': func, 'actions': [{'set_register': 'w31', 'value': 0}]}])
                print('actions: a failing stop: true action stops; unknown keys refused when armed')
                # Four hardware slots, shared with breakpoints.
                five = [{'address': 0xfffff80000001000+i*4} for i in range(5)]
                refused('tracepoints', 'hardware breakpoint slots', tracepoints=five)
                call('breakpoints', addresses=[0xfffff80000002000+i*4 for i in range(3)])
                refused('tracepoints', 'hardware breakpoint slots', tracepoints=five[:2])
                call('tracepoints', tracepoints=five[:1])
                refused('breakpoints', 'hardware breakpoint slots', addresses=[0xfffff80000002000+i*4 for i in range(4)])
                refused('tracepoints', 'length needs', tracepoints=[{'address': func, 'length': 8}])
                assert call('status')['breakpoints'] == [0xfffff80000002000+i*4 for i in range(3)]
                call('breakpoints', addresses=[]); call('tracepoints', tracepoints=[])
                assert call('status')['tracepoints'] == 0
                print('slots: 4 shared with breakpoints, clear errors')
                # Software tracepoints: no slots; the BRKs are only executed, never read.
                loop, func = base+0x214, base+0x240
                every = [loop+i*4 for i in range(7)] + [func, func+4]
                call('write_register', register='pc', value=base+0x200)
                set_ = call('tracepoints', capacity=100000, tracepoints=[{'address': a, 'kind': 'software'} for a in every])
                assert set_['slots'] == 0, set_
                assert all(t['kind'] == 'software' and t['planted'] and t['physical'] == t['address'] for t in set_['tracepoints']), set_
                call('breakpoints', addresses=[0xfffff80000002000+i*4 for i in range(4)])
                call('breakpoints', addresses=[])
                call('resume'); time.sleep(1)
                assert call('status')['state'] == 'Running'
                call('pause')
                assert not [e for e in events if e.get('event') == 'stopped'], events
                records = call('traces', clear=True)['records']
                n = len(records) - len(records) % 9
                assert n >= 270, len(records)
                cycle = [0, 1, 2, 3, 4, 5, 7, 8, 6]
                assert [r['tracepoint'] for r in records[:n]] == cycle*(n//9), [r['tracepoint'] for r in records[:27]]
                assert [r['x'][0] for r in records[:n:9]] == list(range(n//9)), 'a hit was missed'
                nop_ret = 0xd65f03c0d503201f
                assert all(r['x'][3] == 0xd503201f for r in records[3:n:9]), records[3]
                assert all(r['x'][6] == nop_ret for r in records[5:n:9]), records[5]
                assert call('read_physical', address=func, length=8)['hex'] == '1f2003d5c0035fd6'
                assert call('read_physical', address=base+0x910, length=4)['hex'] == '1f2003d5'
                print(f'software: 9 tracepoints, no slots, {n} hits in 1 s in order; the guest reads its instructions, RAM keeps them')
                call('tracepoints', tracepoints=[{'address': func, 'kind': 'software', 'stop': True, 'max_hits': 2}, {'address': func+4, 'kind': 'software'}])
                call('resume'); hit = stop('Tracepoint')
                assert hit['pc'] == func, hit
                call('step'); stepped = stop('SingleStep')
                assert stepped['pc'] == func+4, stepped
                call('resume'); hit = stop('Tracepoint')
                assert hit['pc'] == func, hit
                call('resume'); time.sleep(0.3); call('pause')
                t = call('traces', clear=True)
                first, second = t['tracepoints']
                assert first['hits'] == 2 and not first['armed'] and not first['planted'] and first['pending'] == 'spent', first
                assert second['hits'] > 10 and second['planted'], second
                # The guest rewrites a traced instruction: the BRK follows it.
                call('write_physical', address=func+4, hex='c0035fd6')
                call('write_physical', address=func, hex='08050091')  # add x8, x8, #1
                call('tracepoints', tracepoints=[{'address': func+4, 'kind': 'software'}])
                call('resume'); time.sleep(0.3); call('pause')
                eights = [r['x'][8] for r in call('traces', clear=True)['records']]
                assert len(eights) > 10 and eights == list(range(eights[0], eights[0]+len(eights))), eights[:10]
                call('write_physical', address=func, hex='1f2003d5')
                call('tracepoints', tracepoints=[])
                assert call('read_physical', address=func, length=8)['hex'] == '1f2003d5c0035fd6'
                print('software: stop: true, step and resume past the BRK, max_hits removes it, RAM edits keep it')
                call('shutdown')
                assert process.wait(timeout=10)==0
            finally:
                if process.poll() is None:
                    process.stdin.close()
                    try: process.wait(timeout=10)
                    except subprocess.TimeoutExpired: process.kill();process.wait()


if __name__ == '__main__':
    run(Path(sys.argv[1] if len(sys.argv)>1 else 'target/research/debug/vroom-lab').resolve())
    print('trace smoke OK')

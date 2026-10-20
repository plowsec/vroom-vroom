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

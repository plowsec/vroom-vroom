"""Diskless memory-watch integration test (HVF). Run after research/build.sh."""
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
    with tempfile.TemporaryDirectory(prefix='vroom-watch-') as directory:
        root = Path(directory)
        base = 0x40000000
        image = bytearray(4096)
        struct.pack_into('<I', image, 0, 0x14000080)  # b +0x200
        struct.pack_into('<Q', image, 16, len(image))
        struct.pack_into('<I', image, 56, 0x644d5241)
        # x0 = 0; x1 = base+0x900; x5 = base+0x240; loop: add x0,x0,#1; str x0,[x1];
        # ldr w3,[x5]; str w3,[x1,#16]; ldp x6,x7,[x5]; bl +0x18; b loop; at +0x240: nop; ret
        struct.pack_into('<12I', image, 0x200, 0xd2800000, 0xd2812001, 0xf2a80001, 0xd2804805, 0xf2a80005,
                         0x91000400, 0xf9000020, 0xb94000a3, 0xb9001023, 0xa9401ca6, 0x94000006, 0x17fffffa)
        struct.pack_into('<II', image, 0x240, 0xd503201f, 0xd65f03c0)
        (root/'Image').write_bytes(image)
        (root/'lab.toml').write_text(f'name = "watch-smoke"\ncpus = 1\nmemory_mib = 64\n[display]\ngpu = "virtio2d"\nwidth = 640\nheight = 480\n[boot]\ntype = "kernel"\nkernel = "{root / "Image"}"\n')
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
                process.stdin.write(json.dumps(dict(id=sequence,op=op,**args), separators=(',', ':'))+'\n');process.stdin.flush()
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
            def no_stops():
                assert not [e for e in events if e.get('event') == 'stopped'], events
            def run_for(seconds):
                call('resume'); time.sleep(seconds)
                assert call('status')['state'] == 'Running'
                call('pause'); no_stops()
                return call('traces', clear=True)
            try:
                first = get()
                if first.get('event') != 'ready':
                    errors.seek(0)
                    raise AssertionError(errors.read() or str(first))
                counter, copy, func = base+0x900, base+0x910, base+0x240
                nop_ret = 0xd65f03c0d503201f
                # Software write watch: every store of the counter, none missed; the store
                # to +0x910 on the same page passes.
                set_ = call('hwatch', watches=[{'address': counter, 'length': 8, 'on': 'write', 'kind': 'software', 'name': 'counter'}], capacity=100000)
                assert set_['slots'] == 0 and set_['hwatch'][0]['physical'] == [{'address': counter, 'length': 8}], set_
                t = run_for(1)
                records = t['records']
                assert len(records) >= 100, len(records)
                for r in records:
                    assert r['kind'] == 'hwatch' and r['name'] == 'counter' and r['access'] == 'write' and r['pc'] == base+0x218, r
                    assert r['address'] == counter and r['access_length'] == 8 and r['fault_address'] == counter, r
                    assert int.from_bytes(bytes.fromhex(r['hex']), 'little') == r['x'][0], r
                    assert int.from_bytes(bytes.fromhex(r['before']), 'little') == r['x'][0]-1, r
                    assert 'cntvct' in r and 'time_ns' in r, r
                counts = [r['x'][0] for r in records]
                assert counts == list(range(counts[0], counts[0]+len(counts))), 'a write was missed'
                assert [r['cntvct'] for r in records] == sorted(r['cntvct'] for r in records)
                mw = t['memory_watch']
                assert mw['stops'] >= len(records) and mw['passes'] >= len(records) - 1, mw
                assert call('read_physical', address=copy, length=4)['hex'] == '1f2003d5', 'the passed store happened'
                print(f'software write watch: {len(records)} writes in 1 s, none missed, before/after bytes; same-page store passed ({mw})')
                # Software read watch on code that also runs: LDR (emulated pass) and LDP (stepped
                # pass, size unknown) are seen; the fetch of the nop is not a read.
                call('hwatch', watches=[{'address': func, 'length': 4, 'on': 'read', 'kind': 'software'}])
                records = run_for(0.5)['records']
                assert len(records) >= 20, len(records)
                pcs = [r['pc'] for r in records]
                assert set(pcs) == {base+0x21c, base+0x224}, set(pcs)
                assert all(r['access'] == 'read' for r in records)
                assert [r.get('access_length') for r in records if r['pc'] == base+0x21c][0] == 4
                assert all('access_length' not in r for r in records if r['pc'] == base+0x224)
                regs = call('registers')
                assert (regs['x'][3], regs['x'][6], regs['x'][7]) == (0xd503201f, nop_ret, 0), regs
                assert call('read_physical', address=copy, length=4)['hex'] == '1f2003d5'
                print('software read watch: single loads and pairs on a page that also executes; the guest reads the right values')
                # stop: true: stops after the access, like a breakpoint; resume runs on.
                call('hwatch', watches=[{'address': counter+4, 'length': 1, 'on': 'write', 'kind': 'software', 'stop': True}])
                call('resume'); hit = stop('Hwatch')
                assert hit['pc'] == base+0x21c, hit
                value = call('read_physical', address=counter, length=8)['hex']
                call('resume'); stop('Hwatch')
                assert call('read_physical', address=counter, length=8)['hex'] != value, 'resume ran the loop again'
                call('traces', clear=True)
                print('stop: true stops after the watched write; resume runs past it')
                # Hundreds (stdio takes 64 KiB a request; the agent socket 8 MiB), no slots, any
                # length; a range across pages; one outside RAM refused.
                many = [{'address': base+0x10000+i*16, 'length': 3, 'on': 'access', 'kind': 'software'} for i in range(700)]
                many.append({'address': base+0x3ff0, 'length': 0x40, 'on': 'write', 'kind': 'software'})
                set_ = call('hwatch', watches=many+[{'address': counter, 'on': 'write', 'kind': 'software'}])
                assert set_['slots'] == 0 and len(set_['hwatch']) == 702, len(set_['hwatch'])
                assert set_['hwatch'][700]['physical'] == [{'address': base+0x3ff0, 'length': 0x40}], set_['hwatch'][700]
                records = run_for(0.3)['records']
                assert records and all(r['hwatch'] == 701 for r in records), records[:2]
                refused('hwatch', 'outside RAM', watches=[{'address': 0x10000000, 'length': 8, 'on': 'write', 'kind': 'software'}])
                refused('hwatch', 'length: 1..', watches=[{'address': counter, 'length': 0, 'on': 'write', 'kind': 'software'}])
                refused('hwatch', '8-byte aligned', watches=[{'address': counter+1, 'on': 'write'}])
                print('702 software watches, no slots; outside RAM and bad lengths refused')
                # Hardware hwatches still work beside software ones.
                call('hwatch', watches=[{'address': counter, 'on': 'write'}, {'address': copy, 'length': 4, 'on': 'write', 'kind': 'software'}])
                records = run_for(0.5)['records']
                kinds = {(r['pc'], r['address']) for r in records}
                assert kinds == {(base+0x218, counter), (base+0x220, copy & ~7)}, kinds
                call('hwatch', watches=[])
                before = call('traces')['memory_watch']
                assert run_for(0.3)['records'] == [] and call('traces')['memory_watch'] == before, 'cleared watches no longer fault'
                print('hardware and software hwatches together; [] clears them')
                # Tracepoints: record_if, pid any, status while running, traces memory.
                call('tracepoints', capacity=1000, tracepoints=[{'address': func, 'record_if': {'register': 'x0', 'equals': 2**63}},
                                                               {'address': base+0x21c, 'pid': 'any', 'register': 'x1', 'length': 8, 'max_hits': 3}])
                call('resume'); time.sleep(0.3)
                status = call('tracepoints')
                assert status['state'] == 'Running' and status['tracepoints'][0]['filtered']['record_if'] > 10, status
                call('pause')
                t = call('traces', memory={'at': 'pointer', 'length': 8})
                assert [r['tracepoint'] for r in t['records']] == [1, 1, 1], t['records']
                assert all(r['memory']['address'] == counter and len(r['memory']['hex']) == 16 for r in t['records']), t['records']
                assert t['tracepoints'][0]['hits'] == 0, t['tracepoints'][0]
                call('tracepoints', tracepoints=[]); call('traces', clear=True)
                print('tracepoints: record_if filters (counted), status while running, traces memory reads now')
                # A 16-byte software watch shows all 16 bytes; limit 0 with clear is refused.
                call('hwatch', watches=[{'address': counter, 'length': 16, 'on': 'write', 'kind': 'software'}])
                records = run_for(0.2)['records']
                assert records and all(r['address'] == counter and r['length'] == 16 and len(r['hex']) == 32 for r in records), records[:1]
                assert all(r['hex'][:16] == r['x'][0].to_bytes(8, 'little').hex() for r in records if r['pc'] == base+0x218)
                call('hwatch', watches=[])
                refused('traces', 'omit limit', limit=0, clear=True)
                print('software watch records show the watched bytes (16 here); limit 0 + clear refused')
                # Partial reads and search: past the end of RAM is a hole.
                end = base + (64 << 20)
                r = call('read_virtual', address=end-0x1800, length=0x3000, skip_unmapped=True)
                assert [(x['address'], len(x['hex'])//2) for x in r['runs']] == [(end-0x1800, 0x1800)], r['runs']
                assert r['holes'] == [{'address': end, 'length': 0x1800}], r['holes']
                refused('read_virtual', '', address=end-0x1800, length=0x3000)
                d = call('dump', address=end-0x1800, length=0x3000, skip_unmapped=True)
                assert d['unmapped'] == 0x1800 and Path(d['path']).stat().st_size == 0x3000, d
                Path(d['path']).unlink()
                hits = call('search', address=base, length=0x1000, hex='1f2003d5 c0035fd6')
                assert hits['hits'] == [func] and hits['holes'] == [] and not hits['truncated'], hits
                hits = call('search', address=base, length=0x1000, int=0xd503201f, width=4, align=4)
                assert {func, copy} <= set(hits['hits']), hits
                call('write_physical', address=base+0x2ffc, hex='0badc0de11223344')
                hits = call('search', address=base+0x2000, length=0x2000, hex='0badc0de??223344')
                assert hits['hits'] == [base+0x2ffc], 'a match across pages'
                hits = call('search', address=end-0x1000, length=0x2000, int=0, width=1, limit=3)
                assert hits['hits'] == [end-0x1000, end-0xfff, end-0xffe] and hits['truncated'] and hits['next'] == end-0xffd, hits
                hits = call('search', address=end-0x1000, length=0x2000, text='\u0000never-here')
                assert hits['hits'] == [] and hits['holes'] == [{'address': end, 'length': 0x1000}] and hits['scanned'] == 0x1000, hits
                print('read_virtual/dump skip_unmapped list holes; search: hex with ??, int, across pages, limit/next, holes')
                call('shutdown')
                assert process.wait(timeout=10)==0
            finally:
                if process.poll() is None:
                    process.stdin.close()
                    try: process.wait(timeout=10)
                    except subprocess.TimeoutExpired: process.kill();process.wait()


if __name__ == '__main__':
    run(Path(sys.argv[1] if len(sys.argv)>1 else 'target/research/debug/vroom-lab').resolve())
    print('watch smoke OK')

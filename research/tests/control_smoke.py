"""Diskless ARM64 backend integration test (HVF or KVM). Run after research/build.sh."""
import json
import queue
import struct
import subprocess
import sys
import tempfile
import threading
from pathlib import Path


def run(binary):
    with tempfile.TemporaryDirectory(prefix='vroom-control-') as directory:
        root = Path(directory)
        image = bytearray(4096)
        struct.pack_into('<I', image, 0, 0x14000040)  # b +0x100
        struct.pack_into('<Q', image, 16, len(image))
        struct.pack_into('<I', image, 56, 0x644d5241)
        # mov x0,#1; bl +0x3c; add x0,x0,#10; b .
        struct.pack_into('<IIII', image, 0x100, 0xd2800020, 0x9400000f, 0x91002800, 0x14000000)
        struct.pack_into('<II', image, 0x140, 0x91000800, 0xd65f03c0)  # add x0,x0,#2; ret
        (root/'Image').write_bytes(image)
        (root/'lab.toml').write_text(f'name = "control-smoke"\ncpus = 1\nmemory_mib = 64\n[display]\ngpu = "virtio2d"\nwidth = 640\nheight = 480\n[boot]\ntype = "kernel"\nkernel = "{root / "Image"}"\n')
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
                base = 0x40000000
                def reg(name,value): return call('write_register',register=name,value=value)
                assert reg('x5', 0x123456789abcdef0)['x'][5] == 0x123456789abcdef0
                assert reg('sp',base+0x3000)['sp'] == base+0x3000
                for name,value in [('pc',base+1),('sp',3),('pstate',9),('x31',0)]:
                    try: reg(name,value)
                    except RuntimeError: pass
                    else: raise AssertionError('invalid register edit accepted')
                reg('pc',base+0x104);reg('x0',1)
                own = [0xfffff80000001000]
                call('breakpoints',addresses=own)
                call('step_over');assert stop('StepOver')['pc'] == base+0x108
                assert call('registers')['x'][0] == 3
                assert call('status')['breakpoints'] == own and call('status')['temporary'] is None
                print('step-over: call returns; user breakpoint retained')
                call('read_virtual',address=base+0x140,length=4)  # populate paused-state cache
                call('write_virtual',address=base+0x140,hex='00140091')  # add x0,x0,#5
                assert call('read_virtual',address=base+0x140,length=4)['hex'] == '00140091'
                reg('pc',base+0x104);reg('x0',1)
                call('step_over');stop('StepOver')
                assert call('registers')['x'][0] == 6
                print('memory edit: cache invalidation and already-executed code patch')
                regions=call('regions');end=regions[0][0]+regions[0][1]
                old=call('read_physical',address=end-2,length=2)['hex']
                try: call('write_physical',address=end-2,hex='aabbccdd')
                except RuntimeError: pass
                else: raise AssertionError('write beyond RAM accepted')
                assert call('read_physical',address=end-2,length=2)['hex']==old
                reg('pc',base+0x100)
                call('step_over');assert stop('SingleStep')['pc']==base+0x104
                call('write_virtual',address=base+0x140,hex='00000014') # nonreturning callee
                call('step_over');call('pause')
                assert call('status')['temporary'] is None
                assert call('status')['breakpoints']==own
                # Capacity errors leave every user breakpoint intact.
                own=[0xfffff80000001000+i*4 for i in range(4)]
                call('breakpoints',addresses=own);reg('pc',base+0x104)
                try: call('step_over')
                except RuntimeError as error: assert 'free hardware' in str(error)
                else: raise AssertionError('temporary displaced a user breakpoint')
                assert call('status')['temporary'] is None and call('status')['breakpoints']==own
                print('pause/capacity: temporary cleanup; invalid writes leave RAM intact')
                call('breakpoints',addresses=[]);reg('pc',base+0x10c)  # b .
                call('snapshot',path=str(root/'snap'))
                restored=call('restore',path=str(root/'snap'))
                assert restored['state']=='Paused' and call('status')['state']=='Paused',restored
                try: call('restore',path=str(root/'snap'),resume='yes')
                except RuntimeError as error: assert 'resume' in str(error)
                else: raise AssertionError('non-boolean resume accepted')
                restored=call('restore',path=str(root/'snap'),resume=True)
                assert restored['state']=='Running' and call('status')['state']=='Running',restored
                call('pause');assert call('registers')['pc']==base+0x10c
                print('restore: paused by default, running with resume')
                call('shutdown')
                assert process.wait(timeout=10)==0
            finally:
                if process.poll() is None:
                    process.stdin.close()
                    try: process.wait(timeout=10)
                    except subprocess.TimeoutExpired: process.kill();process.wait()


if __name__ == '__main__':
    run(Path(sys.argv[1] if len(sys.argv)>1 else 'target/research/debug/vroom-lab').resolve())
    print('control smoke OK')

#!/usr/bin/env python3
"""Read-only live guest.download checks. Requires a running Windows guest."""
import json
import socket
import sys

with socket.socket(socket.AF_UNIX) as sock:
    sock.settimeout(300)
    sock.connect(sys.argv[1])
    with sock.makefile('rw') as rpc:
        sequence = 0

        def call(method, params=None, error=False):
            global sequence
            sequence += 1
            rpc.write(json.dumps(dict(jsonrpc='2.0', id=sequence, method=method, params=params or {})) + '\n')
            rpc.flush()
            response = json.loads(rpc.readline())
            assert response['id'] == sequence, response
            if error:
                assert response['error']['code'] == -32603, response
                return response['error']['message']
            assert 'error' not in response, response
            return response['result']

        call('initialize', {'actor': 'download-live-test'})
        commands = call('commands.list', {'category': 'guest'})
        command = next(c for c in commands if c['name'] == 'guest.download')
        assert command['risk'] == 'read'
        assert not any(p['name'] == 'host_path' for p in command['params'])
        path = 'C:\\Windows\\System32\\drivers\\CrowdStrike\\csagent.sys'
        first = call('guest.download', dict(guest_path=path, length=64))
        assert first['length'] == 64 and first['hex'].startswith('4d5a') and not first['eof']
        size = first['file_size']
        for offset in [size, size + 100]:
            end = call('guest.download', dict(guest_path=path, offset=offset, length=64))
            assert end['eof'] and end['length'] == 0 and end['hex'] == ''
            assert end['sha256'] == first['sha256']
        tail = call('guest.download', dict(guest_path=path, offset=size - 7, length=64))
        assert tail['eof'] and tail['length'] == 7 and len(tail['hex']) == 14
        zero = call('guest.download', dict(guest_path=path, length=0))
        assert zero['length'] == 0 and zero['hex'] == '' and not zero['eof']
        assert '65536' in call('guest.download', dict(guest_path=path, length=65537), error=True)
        assert 'unsigned' in call('guest.download', dict(guest_path=path, offset=-1), error=True)
        assert 'host_path' in call('guest.download', dict(guest_path=path, host_path='/tmp/forbidden'), error=True)
        missing = call('guest.download', dict(guest_path='C:\\vroom-nonexistent-download-test.bin'), error=True)
        print(json.dumps(dict(ok=True, file_size=size, sha256=first['sha256'], missing_error=missing,
                              checks=['discovery', 'MZ bytes', 'EOF', 'past EOF', 'short tail', 'zero length',
                                      'oversize', 'negative offset', 'host path denied', 'not found'])))

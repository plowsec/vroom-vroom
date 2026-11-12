#!/usr/bin/env python3
"""Download a running guest's file over the lab RPC socket, without host files.

The output is created exclusively on this client. Each request rereads the file;
its whole-file SHA-256 must remain stable and match the assembled output.
"""
import argparse
import hashlib
import json
import socket
from pathlib import Path


def download(socket_path, guest_path, output):
    with socket.socket(socket.AF_UNIX) as sock:
        sock.settimeout(300)
        sock.connect(socket_path)
        with sock.makefile('rw', encoding='utf-8') as rpc:
            sequence = 0

            def call(method, params):
                nonlocal sequence
                sequence += 1
                rpc.write(json.dumps(dict(jsonrpc='2.0', id=sequence,
                                          method=method, params=params)) + '\n')
                rpc.flush()
                line = rpc.readline()
                if not line:
                    raise RuntimeError('RPC disconnected')
                reply = json.loads(line)
                if reply.get('id') != sequence:
                    raise RuntimeError('unexpected RPC response ID')
                if 'error' in reply:
                    raise RuntimeError(str(reply['error']))
                return reply['result']

            call('initialize', {'actor': 'guest-download'})
            offset, expected, file_size = 0, None, None
            digest = hashlib.sha256()
            # Never overwrite a client's existing file. Failed transfers remain
            # at the destination for diagnosis; success is printed only on hash match.
            with Path(output).open('xb') as destination:
                while True:
                    chunk = call('guest.download', dict(guest_path=guest_path,
                                                       offset=offset, length=65536))
                    data = bytes.fromhex(chunk['hex'])
                    if expected is None:
                        expected, file_size = chunk['sha256'], chunk['file_size']
                    if chunk['sha256'] != expected or chunk['file_size'] != file_size:
                        raise RuntimeError('guest file changed during download')
                    if (chunk['offset'] != offset or chunk['length'] != len(data)
                            or len(data) > 65536 or offset + len(data) > file_size):
                        raise RuntimeError('invalid chunk response')
                    destination.write(data)
                    digest.update(data)
                    offset += len(data)
                    if chunk['eof']:
                        if offset != file_size or digest.hexdigest() != expected:
                            raise RuntimeError('download size/SHA-256 mismatch')
                        break
                    if not data:
                        raise RuntimeError('empty chunk before EOF')
            return dict(path=str(output), size=offset, sha256=expected)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--socket', required=True)
    parser.add_argument('guest_path')
    parser.add_argument('output')
    args = parser.parse_args()
    print(json.dumps(download(args.socket, args.guest_path, args.output)))

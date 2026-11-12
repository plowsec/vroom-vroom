"""Client tests against a local fake JSON-RPC server; no Windows VM needed."""
import hashlib
import importlib.util
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest

spec = importlib.util.spec_from_file_location('guest_download', Path(__file__).parents[1] / 'guest_download.py')
client = importlib.util.module_from_spec(spec)
spec.loader.exec_module(client)


class DownloadTests(unittest.TestCase):
    def transfer(self, data, changing=False, missing=False, existing=False):
        with tempfile.TemporaryDirectory() as directory:
            path = str(Path(directory) / 'rpc.sock')
            output = Path(directory) / 'output'
            if existing:
                output.write_bytes(b'keep')
            with socket.socket(socket.AF_UNIX) as server:
                server.bind(path)
                server.listen(1)

                def serve():
                    with server.accept()[0] as connection:
                        with connection.makefile('rw') as rpc:
                            for line in rpc:
                                request = json.loads(line)
                                if request['method'] == 'initialize':
                                    result = {'actor': 'guest-download'}
                                else:
                                    args = request['params']
                                    offset = args['offset']
                                    chunk = data[offset:offset + args['length']]
                                    result = dict(offset=offset, length=len(chunk),
                                                  file_size=len(data), eof=offset+len(chunk) >= len(data),
                                                  hex=chunk.hex(), sha256=hashlib.sha256(data).hexdigest())
                                    if changing and offset:
                                        result['sha256'] = '0' * 64
                                reply = dict(jsonrpc='2.0', id=request['id'], result=result)
                                if missing and request['method'] != 'initialize':
                                    reply.pop('result')
                                    reply['error'] = dict(code=-32603, message='file not found')
                                rpc.write(json.dumps(reply) + '\n')
                                rpc.flush()

                worker = threading.Thread(target=serve)
                worker.start()
                try:
                    if changing or missing:
                        with self.assertRaises(RuntimeError):
                            client.download(path, 'C:\\test.bin', output)
                    elif existing:
                        with self.assertRaises(FileExistsError):
                            client.download(path, 'C:\\test.bin', output)
                        self.assertEqual(output.read_bytes(), b'keep')
                    else:
                        result = client.download(path, 'C:\\test.bin', output)
                        self.assertEqual(output.read_bytes(), data)
                        self.assertEqual(result['sha256'], hashlib.sha256(data).hexdigest())
                        self.assertEqual(result['size'], len(data))
                finally:
                    worker.join(timeout=5)
                    self.assertFalse(worker.is_alive())

    def test_multiple_chunks(self):
        self.transfer(bytes(range(256)) * 600)

    def test_empty(self):
        self.transfer(b'')

    def test_changed_file(self):
        self.transfer(b'a' * 70000, changing=True)

    def test_missing(self):
        self.transfer(b'', missing=True)

    def test_no_overwrite(self):
        self.transfer(b'new', existing=True)


if __name__ == '__main__':
    unittest.main()

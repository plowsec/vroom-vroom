"""Subprocess transport. Guest serial output never shares this channel."""
import asyncio
import json
import os
import plistlib
import sys
from pathlib import Path


class Client:
    def __init__(self, binary: Path, config: Path, on_event, on_log, restore=None, agent_socket=None):
        self.binary, self.config = binary, config
        self.agent_socket = agent_socket
        self.on_event, self.on_log = on_event, on_log
        self.pending = {}
        self.sequence = 0
        self.process = None
        self.tasks = []
        self.restore = restore
        self.close_lock = asyncio.Lock()

    async def start(self):
        if not self.binary.is_file() or not os.access(self.binary, os.X_OK):
            raise RuntimeError(f'Backend missing: {self.binary}. Run research/build.sh')
        if sys.platform == 'darwin':
            check = await asyncio.create_subprocess_exec(
                '/usr/bin/codesign', '-d', '--entitlements', ':-', str(self.binary),
                stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE)
            stdout, _ = await check.communicate()
            try:
                entitlements = plistlib.loads(stdout)
            except (plistlib.InvalidFileException, ValueError):
                entitlements = {}
            if check.returncode or entitlements.get('com.apple.security.hypervisor') is not True:
                raise RuntimeError('Backend lacks hypervisor entitlement. Run research/build.sh')
        arguments = [str(self.binary), str(self.config)]
        if self.restore:
            arguments.extend(['--restore', str(self.restore)])
        if self.agent_socket:
            arguments.extend(['--agent-socket', str(self.agent_socket)])
        self.process = await asyncio.create_subprocess_exec(
            *arguments, stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE,
            limit=2 * 1024 * 1024)
        self.tasks = [asyncio.create_task(self._read()), asyncio.create_task(self._errors())]

    async def _read(self):
        try:
            while line := await self.process.stdout.readline():
                message = json.loads(line)
                if 'event' in message:
                    self.on_event(message)
                elif future := self.pending.pop(message.get('id'), None):
                    if not future.done():
                        if 'error' in message:
                            future.set_exception(RuntimeError(message['error']))
                        else:
                            future.set_result(message.get('result'))
        except Exception as error:
            self.on_log(str(error))
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(RuntimeError('debugger backend disconnected'))
            self.pending.clear()
            self.on_event({'event': 'disconnected'})

    async def _errors(self):
        while line := await self.process.stderr.readline():
            self.on_log(line.decode(errors='replace').rstrip())

    async def call(self, op, **arguments):
        if not self.process or self.process.returncode is not None:
            raise RuntimeError('debugger backend is not running')
        self.sequence += 1
        ident = self.sequence
        future = asyncio.get_running_loop().create_future()
        self.pending[ident] = future
        try:
            request = json.dumps({'id': ident, 'op': op, **arguments}).encode() + b'\n'
            self.process.stdin.write(request)
            await self.process.stdin.drain()
            return await asyncio.wait_for(future, timeout=240 if op in ('symbols', 'image') else 30)
        finally:
            self.pending.pop(ident, None)

    async def close(self):
        async with self.close_lock:
            if self.process and self.process.returncode is None:
                try:
                    await self.call('shutdown')
                except Exception:
                    self.process.stdin.close()
                try:
                    await asyncio.wait_for(self.process.wait(), 15)
                except asyncio.TimeoutError:
                    self.on_log('Backend did not stop; terminating the disposable VM process')
                    self.process.terminate()
                    try:
                        await asyncio.wait_for(self.process.wait(), 5)
                    except asyncio.TimeoutError:
                        self.process.kill()
                        await self.process.wait()
            for task in self.tasks:
                task.cancel()
            await asyncio.gather(*self.tasks, return_exceptions=True)

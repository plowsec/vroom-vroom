#!/usr/bin/env python3
"""Start the installed Windows lab and verify guest tools and Coop RPC.

Run as the normal host user, not root. No disk creation, rebuild, reset, or
breakpoint clearing. An already paused VM is left alone unless requested.
Applies scripts/set-windows-isolation.ps1 to the guest unless --no-isolate.
"""
import argparse
import fcntl
import json
import os
import secrets
import shlex
from pathlib import Path
import socket
import stat
import subprocess
import sys
import time

ROOT = Path(__file__).resolve().parents[1]


def rpc(path, method, params=None, timeout=10):
    with socket.socket(socket.AF_UNIX) as connection:
        connection.settimeout(timeout)
        connection.connect(str(path))
        with connection.makefile('rw') as stream:
            result = None
            for number, (name, values) in enumerate([
                ('initialize', {'actor': 'lab-startup'}), (method, params or {})
            ], 1):
                stream.write(json.dumps(dict(jsonrpc='2.0', id=number, method=name, params=values)) + '\n')
                stream.flush()
                line = stream.readline()
                if not line:
                    raise RuntimeError('RPC closed the connection')
                reply = json.loads(line)
                if reply.get('id') != number or 'error' in reply:
                    raise RuntimeError(f'RPC {name}: {reply}')
                result = reply['result']
            return result


def wait_for(check, seconds, description):
    deadline = time.monotonic() + seconds
    last = 'not ready'
    while time.monotonic() < deadline:
        try:
            result = check()
            if result:
                return result
        except (OSError, RuntimeError, ValueError) as error:
            last = str(error)
        time.sleep(2)
    raise RuntimeError(f'{description} timed out after {seconds}s: {last}')


def run(command, timeout=20):
    result = subprocess.run(command, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f'{command[0]} failed: {result.stderr.strip() or result.stdout.strip()}')
    return result.stdout


def guest_answers(call):
    """A connected channel is not proof Windows can still execute commands."""
    marker = 'VROOM_ALIVE_' + secrets.token_hex(16)
    result = call('guest.exec', {'command': 'echo ' + marker, 'timeout': 5})
    if (result.get('exit_code') != 0 or result.get('timed_out')
            or result.get('truncated') or result.get('stdout', '').strip() != marker):
        raise RuntimeError(f'Windows did not answer the guest health check: {result}')
    return True


def apply_isolation(call):
    """Apply routing-only isolation in Windows; VirtIO-serial RPC is unaffected."""
    script = (ROOT / 'scripts/set-windows-isolation.ps1').read_text()
    # guest.exec runs PowerShell -Command after its own prelude, so a top-level
    # param block cannot be used; a script block keeps the file unchanged.
    result = call('guest.exec', {'command': '& {\n' + script + '\n}', 'powershell': True, 'timeout': 60})
    if (result.get('exit_code') != 0 or result.get('timed_out')
            or result.get('truncated') or 'ROUTING_ISOLATION_OK' not in result.get('stdout', '')):
        raise RuntimeError(f'Windows routing isolation failed: {result}')
    return True


def coop_call(args, method, params=None):
    remote = ('vroom-rpc --socket ' + shlex.quote(args.coop_socket)
              + ' run ' + shlex.quote(method) + ' ' + shlex.quote(json.dumps(params or {})))
    return json.loads(run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8',
                           args.ssh_host, remote], timeout=20))


def coop_ready(args):
    # Probe an actual JSON-RPC exchange, not just a listening socket.
    return coop_call(args, 'vm.status')


def connect_coop(args, host_socket):
    try:
        status = coop_ready(args)
    except (RuntimeError, ValueError):
        # Remove only a demonstrably stale Unix socket owned by the remote user.
        # Never replace a live listener (including a broken but live proxy).
        cleanup = '''import errno, os, socket, stat, sys
p = sys.argv[1]
try:
 s = os.lstat(p)
except FileNotFoundError:
 sys.exit(0)
if not stat.S_ISSOCK(s.st_mode) or s.st_uid != os.getuid():
 raise SystemExit("refusing to replace a non-socket or another user's socket")
with socket.socket(socket.AF_UNIX) as c:
 c.settimeout(3)
 try:
  c.connect(p)
 except OSError as e:
  if e.errno != errno.ECONNREFUSED: raise
 else:
  raise SystemExit("remote socket has a live listener; refusing to replace it")
os.unlink(p)
'''
        run(['ssh', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8', args.ssh_host,
             'python3 -c ' + shlex.quote(cleanup) + ' ' + shlex.quote(args.coop_socket)])
        run(['ssh', '-fNT', '-o', 'BatchMode=yes', '-o', 'ConnectTimeout=8',
             '-o', 'ExitOnForwardFailure=yes', '-o', 'ServerAliveInterval=15',
             '-o', 'ServerAliveCountMax=3', '-R', f'{args.coop_socket}:{host_socket}', args.ssh_host])
        status = coop_ready(args)
    if status.get('architecture') != 'x86_64' or not status.get('guest_agent', {}).get('connected'):
        raise RuntimeError(f'Coop RPC did not reach a ready x64 guest: {status}')
    # Do this outside tunnel recovery: an unresponsive Windows guest is not a
    # reason to replace an otherwise working SSH listener.
    guest_answers(lambda method, params: coop_call(args, method, params))
    print(f'Coop RPC + Windows response OK: {args.ssh_host}:{args.coop_socket}', flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--config', type=Path, default=ROOT / 'images/windows11-x64.KNZ4j8/windows11.toml')
    parser.add_argument('--lab', type=Path, default=ROOT / 'target/debug/vroom-lab')
    parser.add_argument('--socket', type=Path, default=Path(f'/run/user/{os.getuid()}/vroom-agent.sock'))
    parser.add_argument('--timeout', type=int, default=240, help='guest boot timeout in seconds')
    parser.add_argument('--resume-existing', action='store_true', help='explicitly resume an already paused lab')
    parser.add_argument('--no-coop', action='store_true', help='only start/check the host Windows lab')
    parser.add_argument('--no-isolate', action='store_true', help='leave the guest default routes alone')
    parser.add_argument('--ssh-host', default='coop-crowdstrike')
    parser.add_argument('--coop-socket', default='/home/ubuntu/vroom-agent.sock')
    args = parser.parse_args()
    # These values are used in an SSH remote command / forwarding specification.
    import re
    if not re.fullmatch(r'/[A-Za-z0-9_./-]+', args.coop_socket):
        parser.error('--coop-socket must be an absolute path without shell metacharacters')
    if args.timeout <= 0:
        parser.error('--timeout must be positive')
    if os.getuid() == 0:
        raise RuntimeError('run as the normal host user, not root (RPC is owner-UID restricted)')
    if not args.config.is_file() or not os.access(args.lab, os.X_OK):
        raise RuntimeError('missing config or lab binary; build with cargo build -p vroom-kernel --bins')
    directory = args.socket.parent
    info = directory.stat()
    if info.st_uid != os.getuid() or stat.S_IMODE(info.st_mode) & 0o077:
        raise RuntimeError(f'{directory} must be a private directory owned by this user')
    # Serialize startup without touching the VM state lock or removing RPC sockets.
    with (directory / 'vroom-lab-start.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        started = False
        try:
            status = rpc(args.socket, 'vm.status')
        except (OSError, RuntimeError, ValueError):
            if not os.access('/dev/kvm', os.R_OK | os.W_OK):
                raise RuntimeError('/dev/kvm is unavailable: check KVM modules and user permissions')
            print('Starting Windows lab...', flush=True)
            print(run([str(args.lab.resolve()), str(args.config.resolve()), '--agent-socket',
                       str(args.socket), '--daemon'], timeout=60).strip(), flush=True)
            started = True
            status = wait_for(lambda: rpc(args.socket, 'vm.status'), 30, 'lab RPC')
        if status.get('architecture') != 'x86_64':
            raise RuntimeError(f'not an x64 lab: {status}')
        if status['state'] == 'Paused':
            if not (started or args.resume_existing):
                raise RuntimeError('existing VM is paused; left untouched. Use --resume-existing if appropriate')
            rpc(args.socket, 'vm.resume')
        print('Waiting for Windows to answer a guest command...', flush=True)

        def ready():
            status = rpc(args.socket, 'vm.status')
            if status['state'] != 'Running':
                raise RuntimeError(f'VM is {status["state"]}; not overriding debugger state')
            if not status.get('guest_agent', {}).get('connected'):
                return None
            guest_answers(lambda method, params: rpc(args.socket, method, params, timeout=15))
            return status

        status = wait_for(ready, args.timeout, 'Windows command response')
        commands = rpc(args.socket, 'commands.list', {'category': 'guest'})
        if not any(command['name'] == 'guest.download' for command in commands):
            raise RuntimeError('running lab lacks guest.download; rebuild and restart it')
        print(f'Host RPC + guest execution OK: {args.socket}', flush=True)
        print(f'Guest agent: {status["guest_agent"]["version"]}', flush=True)
        if not args.no_isolate:
            apply_isolation(lambda method, params: rpc(args.socket, method, params, timeout=90))
            print('Windows routing isolation applied: no default routes.', flush=True)
        if not args.no_coop:
            connect_coop(args, args.socket)
        print('READY: Windows running; RPC and guest tools verified.', flush=True)


if __name__ == '__main__':
    try:
        main()
    except (OSError, RuntimeError, ValueError, subprocess.TimeoutExpired) as error:
        print(f'ERROR: {error}', file=sys.stderr)
        print('Existing VM/disks were not reset. Check <socket>.log and the Coop SSH alias.', file=sys.stderr)
        sys.exit(1)

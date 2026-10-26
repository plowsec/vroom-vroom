"""The vroom-lab process behind a lab config: found, started, stopped.

One VM runs once, so a config has one lab and one agent socket, derived from
the config's path: starting a lab that already runs attaches to it instead.
Labs this module starts are daemons (`vroom-lab --daemon`); whoever started
one decides whether it stops with the frontend.
"""
import hashlib
import os
import plistlib
import re
import signal
import socket
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path

REPO = Path(__file__).resolve().parents[3]


def socket_for(config: Path) -> Path:
    digest = hashlib.sha256(str(Path(config).resolve()).encode()).hexdigest()[:12]
    return Path.home() / '.cache' / 'vroom-atui' / f'{digest}.sock'


def accepts(path: Path) -> bool:
    probe = socket.socket(socket.AF_UNIX)
    try:
        probe.connect(str(path))
        return True
    except OSError:
        return False
    finally:
        probe.close()


def lab_binary(explicit=None) -> Path:
    candidates = [explicit, os.environ.get('VROOM_LAB'),
                  REPO / 'target/research/release/vroom-lab', REPO / 'target/research/debug/vroom-lab']
    for candidate in candidates:
        if candidate and Path(candidate).is_file() and os.access(candidate, os.X_OK):
            return Path(candidate)
    raise RuntimeError('vroom-lab missing: run research/build.sh, or set VROOM_LAB')


def check_signed(binary: Path):
    if sys.platform != 'darwin':
        return
    check = subprocess.run(['/usr/bin/codesign', '-d', '--entitlements', ':-', str(binary)],
                           capture_output=True)
    try:
        entitlements = plistlib.loads(check.stdout)
    except (plistlib.InvalidFileException, ValueError):
        entitlements = {}
    if check.returncode or entitlements.get('com.apple.security.hypervisor') is not True:
        raise RuntimeError(f'{binary} lacks the hypervisor entitlement: run research/build.sh')


@dataclass
class Lab:
    socket: Path
    #: The daemon's pid when this process started it; None when it ran already.
    pid: int | None = None

    def stop(self, timeout=15.0):
        """SIGTERM: the lab stops its VM and removes the socket."""
        if self.pid is None:
            return
        pid, self.pid = self.pid, None
        for sig, wait in ((signal.SIGTERM, timeout), (signal.SIGKILL, 5.0)):
            try:
                os.kill(pid, sig)
            except ProcessLookupError:
                return
            deadline = time.monotonic() + wait
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    return
                time.sleep(0.1)


def ensure(config, restore=None, binary=None) -> Lab:
    """The lab for `config`: the running one, or a new daemon.

    A running lab is restored from `restore` over its socket; a new one boots
    from it.
    """
    config = Path(config).expanduser().resolve()
    path = socket_for(config)
    if accepts(path):
        if restore:
            from atui.target.jsonrpc import RpcConnection

            connection = RpcConnection(str(path), actor='vroom-atui', timeout=900)
            try:
                connection.call('vm.restore', {'name': str(restore)})
            finally:
                connection.close()
        return Lab(path)
    binary = lab_binary(binary)
    check_signed(binary)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    arguments = [str(binary), str(config), '--agent-socket', str(path), '--daemon']
    if restore:
        arguments[2:2] = ['--restore', str(restore)]
    started = subprocess.run(arguments, capture_output=True, text=True, timeout=900)
    if started.returncode:
        raise RuntimeError((started.stderr or started.stdout).strip() or f'vroom-lab exited with {started.returncode}')
    match = re.search(r'\(pid (\d+)\)', started.stdout)
    return Lab(path, int(match.group(1)) if match else None)

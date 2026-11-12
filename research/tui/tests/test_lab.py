import subprocess
import sys
import threading
from pathlib import Path

import pytest

from fake_lab import LabServer
from vroom_atui import lab as lab_module
from vroom_atui import launch
from vroom_atui.target import open_vroom


@pytest.fixture
def server():
    server = LabServer()
    yield server
    server.close()


@pytest.fixture
def daemon():
    """A process standing in for a lab daemon this process started."""
    process = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(60)'])
    # A real daemon is nobody's child here; reap this one so it is gone, not a zombie.
    threading.Thread(target=process.wait, daemon=True).start()
    yield process
    if process.poll() is None:
        process.kill()
    process.wait()


def test_socket_paths_fit_sun_path():
    path = lab_module.socket_for(Path('/a/very/long/directory/name/that/goes/on/and/on/lab.toml'))
    assert len(str(path)) < 104 and path == lab_module.socket_for(Path('/a/very/long/directory/name/that/goes/on/and/on/lab.toml'))


def test_a_running_lab_is_reused_and_restored(server, monkeypatch, tmp_path):
    monkeypatch.setattr(lab_module, 'socket_for', lambda config: Path(server.path))
    monkeypatch.setattr(lab_module, 'lab_binary', lambda explicit=None: pytest.fail('started a second lab'))
    lab = lab_module.ensure(tmp_path / 'lab.toml', restore='snap-1')
    assert lab.pid is None and lab.socket == Path(server.path)
    assert ('vm.restore', {'name': 'snap-1'}) in server.lab.calls


def test_the_launcher_opens_angr_tui_on_the_target(monkeypatch, tmp_path):
    calls = []
    monkeypatch.setattr(launch.os, 'execv', lambda path, argv: calls.append(argv[1:]))
    config = tmp_path / 'lab.toml'
    launch.main([str(config), '--restore', 'snap-1', '--keep', '--seek', 'nt!KeBugCheckEx'])
    launch.main([str(tmp_path / 'work.atui')])
    launch.main([str(tmp_path / 'lab.sock'), '--binary', str(tmp_path / 'driver.sys'), '--lab-binary', '/x/vroom-lab'])
    assert calls == [
        ['-m', 'atui', '--target', f'vroom:{config},restore=snap-1,keep', '--seek', 'nt!KeBugCheckEx'],
        ['-m', 'atui', str(tmp_path / 'work.atui'), '--resume'],
        ['-m', 'atui', str(tmp_path / 'driver.sys'), '--target', f'vroom:{tmp_path / "lab.sock"}'],
    ]
    with pytest.raises(SystemExit):
        launch.main([str(tmp_path / 'lab.sock'), '--keep'])


def test_an_owned_lab_stops_with_the_process(daemon):
    lab = lab_module.Lab(Path('/nowhere'), daemon.pid)
    lab.own()
    assert lab in lab_module._owned
    lab_module._stop_owned()
    assert daemon.wait(5) is not None and lab.pid is None and lab not in lab_module._owned


def _started(monkeypatch, server, daemon):
    monkeypatch.setattr('vroom_atui.target.ensure', lambda *args: lab_module.Lab(Path(server.path), daemon.pid))


def test_a_lab_the_provider_started_stops_on_detach_unless_kept(server, daemon, monkeypatch):
    _started(monkeypatch, server, daemon)
    target = open_vroom('lab.toml')
    assert target.lab in lab_module._owned
    target.detach()
    assert daemon.wait(5) is not None and not lab_module._owned


def test_a_kept_lab_outlives_the_frontend(server, daemon, monkeypatch):
    _started(monkeypatch, server, daemon)
    target = open_vroom('lab.toml,keep')
    assert target.lab is None and not lab_module._owned
    target.detach()
    lab_module._stop_owned()
    assert daemon.poll() is None


def test_a_lab_started_for_a_failed_attach_is_stopped(server, daemon, monkeypatch):
    _started(monkeypatch, server, daemon)

    def refuse(*args, **kwargs):
        raise RuntimeError('refused')

    monkeypatch.setattr('vroom_atui.target.VroomTarget.__init__', refuse)
    with pytest.raises(RuntimeError, match='refused'):
        open_vroom('lab.toml')
    assert daemon.wait(5) is not None and not lab_module._owned

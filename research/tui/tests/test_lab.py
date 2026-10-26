from pathlib import Path

import pytest

from fake_lab import LabServer
from vroom_atui import lab as lab_module
from vroom_atui.launch import kernel_image


@pytest.fixture
def server():
    server = LabServer()
    yield server
    server.close()


def test_socket_paths_fit_sun_path():
    path = lab_module.socket_for(Path('/a/very/long/directory/name/that/goes/on/and/on/lab.toml'))
    assert len(str(path)) < 104 and path == lab_module.socket_for(Path('/a/very/long/directory/name/that/goes/on/and/on/lab.toml'))


def test_a_running_lab_is_reused_and_restored(server, monkeypatch, tmp_path):
    monkeypatch.setattr(lab_module, 'socket_for', lambda config: Path(server.path))
    monkeypatch.setattr(lab_module, 'lab_binary', lambda explicit=None: pytest.fail('started a second lab'))
    lab = lab_module.ensure(tmp_path / 'lab.toml', restore='snap-1')
    assert lab.pid is None and lab.socket == Path(server.path)
    assert ('vm.restore', {'name': 'snap-1'}) in server.lab.calls


def test_the_launcher_opens_the_kernel_s_rva_copy(server):
    path = kernel_image(Path(server.path))
    assert path.name.endswith('.rva.exe') and path.read_bytes()[:2] == b'MZ'
    server.lab.state = 'Running'
    assert kernel_image(Path(server.path)) == path, 'a running lab reuses the last export'
    Path(f'{server.path}.kernel').unlink()
    with pytest.raises(RuntimeError, match='pause it'):
        kernel_image(Path(server.path))

import argparse

import pytest

from atui.cli import _open_session
from atui.core.session.session import Session
from atui.target.attach import attach
from atui.target.resume import checkpoint, resume

from fake_lab import KERNEL, LabServer


@pytest.fixture
def server():
    server = LabServer()
    yield server
    server.close()


def test_a_checkpoint_is_a_lab_snapshot_and_resume_restores_it(binary, server, tmp_path):
    session = Session(binary, cache_root=tmp_path / 'cache')
    target = attach(session, 'vroom', server.path)
    target.add_breakpoint(KERNEL + 0x40)
    project = tmp_path / 'work.atui'
    result = checkpoint(session, 'before-ioctl', project=project, view=KERNEL + 0x1000)
    assert result['snapshot'] == {'name': 'before-ioctl', 'path': '/snapshots/before-ioctl'}
    assert ('vm.snapshot', {'name': 'before-ioctl'}) in server.lab.calls
    session.close()
    server.lab.breakpoints = []

    opened = _open_session(argparse.Namespace(binary=str(project), no_cache=False, verify_hash=False,
                                              diagnostic_log=None), tmp_path / 'cache')
    reopened = opened.session
    try:
        report = resume(reopened)
        assert report.attached == ('vroom',) and report.restored == ('vroom',) and not report.errors
        assert ('vm.restore', {'name': 'before-ioctl'}) in server.lab.calls
        assert server.lab.breakpoints == [KERNEL + 0x40], 'armed by location, after the restore'
        assert report.view == KERNEL + 0x1000
        assert reopened.targets.get('vroom').spec == server.path
    finally:
        reopened.close()

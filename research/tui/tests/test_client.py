import sys
from pathlib import Path

import pytest

from vroom_tui.client import Client


async def test_missing_backend_reports_build_command(tmp_path):
    client = Client(tmp_path/'missing', Path('unused'), lambda _: None, lambda _: None)
    with pytest.raises(RuntimeError, match='research/build.sh'):
        await client.start()
    assert client.process is None


@pytest.mark.skipif(sys.platform != 'darwin', reason='macOS entitlement')
async def test_unsigned_backend_is_rejected_before_launch(tmp_path):
    executable = tmp_path/'unsigned'
    executable.write_text('#!/bin/sh\nexit 99\n')
    executable.chmod(0o755)
    client = Client(executable, Path('unused'), lambda _: None, lambda _: None)
    with pytest.raises(RuntimeError, match='hypervisor entitlement'):
        await client.start()
    assert client.process is None

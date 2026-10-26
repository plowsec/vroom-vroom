import shutil
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

HERE = Path(__file__).parent
sys.path.insert(0, str(HERE))


@pytest.fixture(autouse=True)
def atui_state(tmp_path_factory, monkeypatch):
    """angr_tui's settings and caches stay out of the user's directories."""
    monkeypatch.setenv('ATUI_STATE_DIR', str(tmp_path_factory.mktemp('atui-state')))


@pytest.fixture
def session_free_dispatcher():
    from atui.services.commands import Dispatcher

    return Dispatcher(SimpleNamespace())


@pytest.fixture(scope='session')
def binary(tmp_path_factory):
    cc = shutil.which('cc') or shutil.which('gcc')
    if cc is None:
        pytest.skip('no C compiler')
    source = tmp_path_factory.mktemp('src') / 'leaf.c'
    source.write_text('int leaf(int x) { return x * 3 + 1; }\nint root(int x) { return leaf(x) + leaf(x + 1); }\n')
    out = source.with_suffix('.so')
    subprocess.run([cc, '-shared', '-fPIC', '-O0', '-o', str(out), str(source)], check=True)
    return out


@pytest.fixture
def app(binary, tmp_path):
    from atui.core.session.session import Session
    from atui.ui.app import AtuiApp

    session = Session(binary, cache_root=tmp_path / 'cache')
    yield AtuiApp(session)
    session.close()

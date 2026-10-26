"""`vroom-atui`: angr_tui on the lab's kernel, with the lab attached.

angr_tui opens a binary: this exports the running kernel's image (an RVA
analysis copy) and opens that, attached to the lab as target `vroom`. A
lab this starts stops when angr_tui quits, unless --keep; a lab that ran
already keeps running. With the lab running (not paused), the kernel image
of the last launch is reused.
"""
import argparse
import os
import sys
from pathlib import Path

from .analysis import prepare_rva_image
from .lab import accepts, ensure


def kernel_image(socket_path: Path) -> Path:
    from atui.target.jsonrpc import RemoteError, RpcConnection

    remembered = Path(f'{socket_path}.kernel')
    connection = RpcConnection(str(socket_path), actor='vroom-atui', timeout=300)
    try:
        status = connection.call('vm.status')
        if status.get('state') != 'Paused':
            if remembered.is_file() and Path(remembered.read_text()).is_file():
                return Path(remembered.read_text())
            raise RuntimeError('the lab runs and no kernel image was exported yet: pause it, or pass --binary')
        try:
            kernel = connection.call('kernel.info', {'cpu': 0})
            exported = connection.call('kernel.export', {'cpu': 0, 'base': kernel['base'], 'allow_missing': True})
        except RemoteError as error:
            raise RuntimeError(f'{error} (boot or restore the guest first, or pass --binary)') from error
    finally:
        connection.close()
    path, _ = prepare_rva_image(exported, allow_unrelocated=True)
    remembered.write_text(str(path))
    return path


def main(argv=None):
    parser = argparse.ArgumentParser(prog='vroom-atui', description=__doc__.splitlines()[0])
    parser.add_argument('lab', type=Path, help='lab config (.toml) or a running lab\'s agent socket')
    parser.add_argument('--restore', help='snapshot name or folder')
    parser.add_argument('--lab-binary', type=Path, help='vroom-lab (default: $VROOM_LAB, target/research/)')
    parser.add_argument('--keep', action='store_true', help='leave a lab started here running on exit')
    parser.add_argument('--binary', type=Path, help='open this binary instead of the kernel image')
    args, rest = parser.parse_known_args(argv)
    try:
        if args.lab.suffix == '.toml':
            lab = ensure(args.lab, args.restore, args.lab_binary)
            socket_path, owned = lab.socket, lab.pid is not None and not args.keep
            print(f'vroom-atui: lab at {socket_path}' + ('' if lab.pid is None else f' (pid {lab.pid})'), file=sys.stderr)
        elif accepts(args.lab):
            if args.restore:
                parser.error('--restore needs a lab config')
            socket_path, owned = args.lab, False
        else:
            parser.error(f'{args.lab}: not a lab config, and no lab answers there')
        binary = args.binary or kernel_image(socket_path)
    except (RuntimeError, OSError) as error:
        print(f'vroom-atui: {error}', file=sys.stderr)
        return 1
    target = f'vroom:{socket_path}' + (',stop' if owned else '')
    os.execv(sys.executable, [sys.executable, '-m', 'atui', str(binary), '--target', target, *rest])

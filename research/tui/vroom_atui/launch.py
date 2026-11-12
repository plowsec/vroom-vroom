"""`vroom-atui`: angr_tui attached to a lab, opened on the module it is stopped in.

The target spec carries the lab's lifetime: a lab the provider starts stops
with angr_tui unless --keep; a lab that ran already keeps running. A project
(.atui) resumes: its targets re-attach and its checkpoint is restored.
"""
import argparse
import os
import sys
from pathlib import Path


def spec(lab: Path, restore=None, lab_binary=None, keep=False) -> str:
    """The `vroom:` target spec for these options."""
    words = [str(lab)]
    if restore:
        words.append(f'restore={restore}')
    if lab_binary:
        words.append(f'lab={lab_binary}')
    if keep:
        words.append('keep')
    return ','.join(words)


def main(argv=None):
    parser = argparse.ArgumentParser(prog='vroom-atui', description=__doc__.splitlines()[0])
    parser.add_argument('lab', type=Path,
                        help='lab config (.toml), a running lab\'s agent socket, or a project (.atui) to resume')
    parser.add_argument('--restore', help='snapshot name or folder')
    parser.add_argument('--lab-binary', type=Path, help='vroom-lab (default: $VROOM_LAB, target/research/)')
    parser.add_argument('--keep', action='store_true', help='leave a lab started here running on exit')
    parser.add_argument('--binary', type=Path, help='open this binary instead of the module the lab is stopped in')
    args, rest = parser.parse_known_args(argv)
    if args.lab.suffix == '.atui':
        # A project re-attaches its own targets and restores its checkpoint.
        return os.execv(sys.executable, [sys.executable, '-m', 'atui', str(args.lab), '--resume', *rest])
    config = args.lab.suffix == '.toml'
    if not config and (args.restore or args.keep):
        parser.error('--restore and --keep need a lab config')
    target = 'vroom:' + spec(args.lab.expanduser().resolve(), args.restore, args.lab_binary if config else None,
                             args.keep)
    binary = [str(args.binary)] if args.binary else []
    return os.execv(sys.executable, [sys.executable, '-m', 'atui', *binary, '--target', target, *rest])

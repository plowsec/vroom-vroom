"""vroom-lab in angr_tui: a live Windows ARM64 kernel as an execution target."""
from atui.plugins import PaneSpec, PluginSpec, TargetProvider


def _open(spec, session):
    from .target import open_vroom

    return open_vroom(spec, session)


def _pane(name):
    def factory(app, target):
        from . import panes

        return getattr(panes, name)(target)

    return factory


PANES = (
    ('vroom-modules', 'Modules', 'ModulesPane', 'f3'),
    ('vroom-processes', 'Processes', 'ProcessesPane', 'f4'),
    ('vroom-stack', 'Stack', 'StackPane', 'f10'),
    ('vroom-event', 'Callback event', 'EventPane', 'f11'),
    ('vroom-callbacks', 'Callbacks', 'CallbacksPane', ''),
    ('vroom-peb', 'PEB', 'PebPane', ''),
    ('vroom-faults', 'Faults', 'FaultsPane', ''),
    ('vroom-traces', 'Traces', 'TracesPane', ''),
    ('vroom-snapshots', 'Snapshots', 'SnapshotsPane', ''),
    ('vroom-coverage', 'Coverage', 'CoveragePane', ''),
)

PLUGIN = PluginSpec(
    plugin_id='vroom',
    version='0.1.0',
    description='vroom-lab: live Windows ARM64 kernel debugging',
    target_providers=(TargetProvider('vroom', 'vroom-lab', _open,
                                     'LAB.toml[,restore=SNAPSHOT][,keep] or an agent socket[,stop]'),),
    panes=tuple(PaneSpec(pane_id, title, _pane(name), key=key, providers=('vroom',))
                for pane_id, title, name, key in PANES),
)

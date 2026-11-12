"""Startup health-check tests; do not launch or change a real VM."""
import argparse
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('startup', Path(__file__).parents[2] / 'scripts/start-windows-lab.py')
startup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(startup)


class StartupTests(unittest.TestCase):
    def args(self):
        return argparse.Namespace(ssh_host='coop-test', coop_socket='/home/ubuntu/vroom-agent.sock')

    def status(self):
        return {'architecture': 'x86_64', 'guest_agent': {'connected': True}}

    def test_healthy_forward_is_reused(self):
        with patch.object(startup, 'coop_ready', return_value=self.status()), patch.object(startup, 'guest_answers', return_value=True) as probe, patch.object(startup, 'run') as run:
            startup.connect_coop(self.args(), Path('/run/user/1000/vroom-agent.sock'))
            run.assert_not_called()
            probe.assert_called_once()

    def test_missing_forward_is_started_and_verified(self):
        with patch.object(startup, 'coop_ready', side_effect=[RuntimeError('missing'), self.status()]), patch.object(startup, 'guest_answers', return_value=True), patch.object(startup, 'run', return_value='') as run:
            startup.connect_coop(self.args(), Path('/run/user/1000/vroom-agent.sock'))
            self.assertEqual(run.call_count, 2)
            cleanup, forward = [call.args[0] for call in run.call_args_list]
            self.assertIn('ECONNREFUSED', cleanup[-1])
            self.assertIn('refusing to replace', cleanup[-1])
            self.assertIn('-fNT', forward)
            self.assertIn('/home/ubuntu/vroom-agent.sock:/run/user/1000/vroom-agent.sock', forward)

    def test_live_broken_listener_is_not_replaced(self):
        with patch.object(startup, 'coop_ready', side_effect=RuntimeError('reset')), patch.object(startup, 'run', side_effect=RuntimeError('live listener')) as run:
            with self.assertRaisesRegex(RuntimeError, 'live listener'):
                startup.connect_coop(self.args(), Path('/run/user/1000/vroom-agent.sock'))
            self.assertEqual(run.call_count, 1)

    def test_wrong_backend_is_rejected(self):
        with patch.object(startup, 'coop_ready', return_value={'architecture': 'aarch64'}), patch.object(startup, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'ready x64'):
                startup.connect_coop(self.args(), Path('/run/user/1000/vroom-agent.sock'))
            run.assert_not_called()

    def test_wait_retries(self):
        with patch.object(startup.time, 'sleep') as sleep:
            values = iter([False, OSError('booting'), {'ready': True}])

            def check():
                value = next(values)
                if isinstance(value, Exception):
                    raise value
                return value

            self.assertEqual(startup.wait_for(check, 10, 'test'), {'ready': True})
            self.assertEqual(sleep.call_count, 2)

    def test_wait_times_out(self):
        with patch.object(startup.time, 'monotonic', side_effect=[0, 2]):
            with self.assertRaisesRegex(RuntimeError, 'timed out'):
                startup.wait_for(lambda: False, 1, 'test')

    def test_failed_command_reports_error(self):
        with patch.object(startup.subprocess, 'run', return_value=subprocess.CompletedProcess(['ssh'], 1, '', 'unreachable')):
            with self.assertRaisesRegex(RuntimeError, 'unreachable'):
                startup.run(['ssh'])

    def test_guest_answers_requires_fresh_exact_response(self):
        def echo(method, params):
            self.assertEqual(method, 'guest.exec')
            self.assertEqual(params['timeout'], 5)
            return {'exit_code': 0, 'stdout': params['command'][5:] + '\r\n'}

        self.assertTrue(startup.guest_answers(echo))

    def test_guest_answers_rejects_stale_failed_or_partial_response(self):
        for result in [
            {'exit_code': 0, 'stdout': 'VROOM_STARTUP_OK'},
            {'exit_code': 0, 'stdout': ''},
            {'exit_code': 1, 'stdout': 'VROOM_ALIVE_test'},
            {'exit_code': 0, 'stdout': 'VROOM_ALIVE_test', 'timed_out': True},
            {'exit_code': 0, 'stdout': 'VROOM_ALIVE_test', 'truncated': True},
            {'exit_code': 0, 'stdout': 'prefix VROOM_ALIVE_test suffix'},
        ]:
            with self.subTest(result=result), patch.object(startup.secrets, 'token_hex', return_value='test'):
                with self.assertRaisesRegex(RuntimeError, 'did not answer'):
                    startup.guest_answers(lambda method, params: result)

    def test_isolation_runs_script_through_powershell(self):
        calls = []

        def execute(method, params):
            calls.append((method, params))
            return {'exit_code': 0, 'stdout': 'ROUTING_ISOLATION_OK\r\n'}

        self.assertTrue(startup.apply_isolation(execute))
        method, params = calls[0]
        self.assertEqual(method, 'guest.exec')
        self.assertTrue(params['powershell'])
        script = startup.ROOT.joinpath('scripts/set-windows-isolation.ps1').read_text()
        self.assertIn('& {', params['command'])
        self.assertIn(script, params['command'])

    def test_isolation_requires_successful_marker(self):
        for result in [
            {'exit_code': 1, 'stdout': 'ROUTING_ISOLATION_OK'},
            {'exit_code': 0, 'stdout': ''},
            {'exit_code': 0, 'stdout': 'ROUTING_ISOLATION_OK', 'timed_out': True},
            {'exit_code': 0, 'stdout': 'ROUTING_ISOLATION_OK', 'truncated': True},
        ]:
            with self.subTest(result=result):
                with self.assertRaisesRegex(RuntimeError, 'routing isolation failed'):
                    startup.apply_isolation(lambda method, params: result)

    def test_unresponsive_guest_does_not_replace_tunnel(self):
        with patch.object(startup, 'coop_ready', return_value=self.status()), patch.object(startup, 'guest_answers', side_effect=RuntimeError('Windows unresponsive')), patch.object(startup, 'run') as run:
            with self.assertRaisesRegex(RuntimeError, 'unresponsive'):
                startup.connect_coop(self.args(), Path('/run/user/1000/vroom-agent.sock'))
            run.assert_not_called()


if __name__ == '__main__':
    unittest.main()

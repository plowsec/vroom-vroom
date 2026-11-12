"""Bootstrap regression tests, including the real CLI against loopback only."""
import base64
import http.server
import importlib.util
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import tempfile
import threading
import unittest
from urllib.parse import parse_qs
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("expiry_fixtures", Path(__file__).with_name("test_aws_expiry_self_test.py"))
FIXTURES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURES)
host = FIXTURES.host


class BootstrapTests(unittest.TestCase):
    def prepared(self):
        return FIXTURES.ExpirySelfTestTests().prepared()

    def test_user_data_is_script_not_encoded_text(self):
        value = self.prepared()
        script = host.instance_request(value)["UserData"]
        self.assertTrue(script.startswith("#!/bin/bash\nset -eu\n"))
        self.assertIn("trap '/sbin/shutdown -h now' ERR", script)
        self.assertIn(host.deadlines(value)[1].strftime("%Y-%m-%d %H:%M:%S UTC"), script)
        self.assertLess(len(script.encode()), 16 * 1024)

    @unittest.skipUnless(shutil.which("aws"), "installed AWS CLI required for loopback serialization test")
    def test_real_cli_encodes_user_data_exactly_once(self):
        captured = []

        class Receiver(http.server.BaseHTTPRequestHandler):
            def do_POST(self):
                captured.append(parse_qs(self.rfile.read(int(self.headers["Content-Length"])).decode()))
                body = b'<Response><Errors><Error><Code>OfflineProbe</Code><Message>local only</Message></Error></Errors></Response>'
                self.send_response(400)
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def log_message(self, *_args):
                pass

        payload = host.instance_request(self.prepared())
        with http.server.HTTPServer(("127.0.0.1", 0), Receiver) as server, tempfile.TemporaryDirectory() as directory:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                # No profile, signing, credentials, metadata, proxies or AWS endpoint.
                env = {"PATH": os.environ.get("PATH", ""), "HOME": directory,
                       "AWS_CONFIG_FILE": os.devnull, "AWS_SHARED_CREDENTIALS_FILE": os.devnull,
                       "AWS_EC2_METADATA_DISABLED": "true", "AWS_MAX_ATTEMPTS": "1", "AWS_PAGER": ""}
                args = [shutil.which("aws"), "--region", "eu-north-1", "--no-sign-request",
                    "--no-cli-pager", "--no-cli-auto-prompt", "--cli-binary-format", "base64",
                    "--endpoint-url", f"http://127.0.0.1:{server.server_port}", "--cli-connect-timeout", "2",
                    "--cli-read-timeout", "2", "ec2", "run-instances", "--cli-input-json"]
                for content in [payload["UserData"], base64.b64encode(payload["UserData"].encode()).decode()]:
                    result = subprocess.run(args + [json.dumps(dict(payload, UserData=content))],
                        env=env, capture_output=True, text=True, timeout=15)
                    self.assertNotEqual(result.returncode, 0)
            finally:
                server.shutdown()
                thread.join(timeout=2)
        self.assertEqual(len(captured), 2)
        self.assertEqual(base64.b64decode(captured[0]["UserData"][0], validate=True).decode(), payload["UserData"])
        # Regression: the old pre-encoded input acquires a second encoding.
        encoded_twice = base64.b64decode(captured[1]["UserData"][0], validate=True)
        self.assertNotEqual(encoded_twice.decode(), payload["UserData"])
        self.assertEqual(base64.b64decode(encoded_twice, validate=True).decode(), payload["UserData"])

    def run_check(self, overrides=None, marker=None, actual_offset=0, guard_remaining=1):
        value = self.prepared()
        expected = int(host.deadlines(value)[1].timestamp())
        properties = {
            ("timer", "LoadState"): "loaded", ("timer", "ActiveState"): "active",
            ("timer", "Transient"): "yes", ("timer", "Unit"): "vroom-pilot-expiry.service",
            ("timer", "NextElapseUSecRealtime"): "Sat 2026-11-07 12:30:00 UTC",
            ("service", "LoadState"): "loaded", ("service", "Transient"): "yes",
            ("service", "User"): "", ("service", "Result"): "success",
            ("service", "ExecStart"): "{ path=/sbin/shutdown ; argv[]=/sbin/shutdown -h now ; ignore_errors=no ; start_time=[n/a] ; }",
        }
        properties.update(overrides or {})

        def command(args, **_kwargs):
            if args[0] == "date":
                return str(expected + actual_offset)
            return properties[(args[2].rsplit(".", 1)[1], args[3].removeprefix("--property="))]

        script = host.bootstrap_check(value).split("\n", 1)[1].rsplit("\nVROOM_BOOTSTRAP_CHECK", 1)[0]
        with patch("pathlib.Path.is_symlink", return_value=False), \
                patch("pathlib.Path.read_text", return_value=marker or value["run_id"]), \
                patch("os.stat", return_value=type("Stat", (), {"st_mode": stat.S_IFCHR})()), \
                patch("subprocess.check_output", side_effect=command), \
                patch("time.time", return_value=host.deadlines(value)[0].timestamp() - guard_remaining), \
                patch("builtins.print"):
            exec(compile(script, "bootstrap-check", "exec"), {})

    def test_observed_timer_and_marker_pass(self):
        self.run_check()

    def test_failed_check_never_reaches_subsequent_shell_workload(self):
        with tempfile.TemporaryDirectory() as directory:
            # A failed interpreter/check must halt the caller even without set -e.
            python = Path(directory) / "python3"
            python.write_text("#!/bin/sh\nexit 19\n")
            python.chmod(0o700)
            result = subprocess.run(["/bin/sh"], input=host.bootstrap_check(self.prepared()) + "echo WORKLOAD_RAN\n",
                text=True, capture_output=True, timeout=5, env={"PATH": directory})
        self.assertEqual(result.returncode, 19)
        self.assertNotIn("WORKLOAD_RAN", result.stdout)

    def test_missing_changed_or_failed_timer_and_wrong_marker_fail(self):
        for properties in [{("timer", "ActiveState"): "inactive"}, {("timer", "Unit"): "other.service"},
                           {("service", "ExecStart"): "{ path=/bin/true ; argv[]=/bin/true ; ignore_errors=no ; }"},
                           {("service", "Result"): "exit-code"}, {("service", "User"): "nobody"}]:
            with self.subTest(properties=properties), self.assertRaises(AssertionError):
                self.run_check(properties)
        with self.assertRaises(AssertionError):
            self.run_check(marker="different-run")
        with self.assertRaises(AssertionError):
            self.run_check(actual_offset=60)
        with self.assertRaises(AssertionError):
            self.run_check(guard_remaining=0)


if __name__ == "__main__":
    unittest.main()

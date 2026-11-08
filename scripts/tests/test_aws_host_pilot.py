"""Failure-path tests; AWS is replaced with a deterministic fake."""
import importlib.util
from datetime import datetime, timedelta, timezone
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch


def load(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).parents[1] / filename)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


host = load("host_pilot", "aws-host-pilot.py")
reaper = load("pilot_reaper", "aws-pilot-reaper.py")
RUN = "vroom-pilot-0123456789ab"
ACCOUNT = "123456789012"
NOW = datetime(2026, 11, 7, 12, tzinfo=timezone.utc)


def manifest():
    return {"run_id": RUN, "account": ACCOUNT, "status": "running", "run_requested": True,
            "launch_response_received": True, "instance_ids": ["i-owned"],
            "deadline": (datetime.now(timezone.utc) + timedelta(minutes=20)).isoformat()}


def instance(state="running", run_id=RUN):
    return {"InstanceId": "i-owned", "State": {"Name": state}, "Tags": host.tags(run_id)}


class FakeAws:
    profile = "test"
    region = "eu-north-1"

    def __init__(self, by_tags=None, by_id=None, volumes=None):
        self.by_tags = by_tags or []
        self.by_id = by_id or []
        self.volumes = volumes or []
        self.calls = []

    def call(self, service, operation, payload=None, **kwargs):
        self.calls.append((service, operation, payload))
        if operation == "get-caller-identity":
            return {"Account": ACCOUNT, "Arn": f"arn:aws:sts::{ACCOUNT}:assumed-role/role/session"}
        if operation == "describe-instances":
            items = self.by_id if "InstanceIds" in payload else self.by_tags
            return {"Reservations": [{"Instances": items}]}
        if operation == "describe-volumes":
            return {"Volumes": self.volumes}
        if operation == "terminate-instances":
            return {}
        raise AssertionError(f"unexpected API: {service} {operation}")


class FakeEc2:
    def __init__(self, items, dry_run_error="DryRunOperation"):
        self.items, self.calls, self.dry_run_error = items, [], dry_run_error

    def get_paginator(self, operation):
        return self

    def paginate(self, **kwargs):
        return [{"Reservations": [{"Instances": self.items}]}]

    def terminate_instances(self, **kwargs):
        self.calls.append(kwargs)
        if kwargs.get("DryRun"):
            error = RuntimeError("test dry run")
            error.response = {"Error": {"Code": self.dry_run_error}}
            raise error


class CleanupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.path = Path(self.temp.name) / "run.json"

    def tearDown(self):
        self.temp.cleanup()

    def test_empty_tag_query_cannot_hide_live_known_instance(self):
        aws = FakeAws(by_id=[instance()])
        with self.assertRaisesRegex(host.PilotError, "terminal"):
            host.cleanup(aws, manifest(), self.path)
        self.assertEqual([c[1] for c in aws.calls if c[1].startswith("terminate")], ["terminate-instances"])
        self.assertFalse(any(c[0] in ("lambda", "events", "iam") for c in aws.calls))

    def test_missing_known_id_retains_guard(self):
        aws = FakeAws()
        with self.assertRaisesRegex(host.PilotError, "positively observed"):
            host.cleanup(aws, manifest(), self.path)
        self.assertFalse(any(c[1].startswith("delete") for c in aws.calls))

    def test_ambiguous_launch_without_id_never_disarms_guard(self):
        value = manifest()
        value.update(instance_ids=[], launch_response_received=False)
        aws = FakeAws()
        with self.assertRaisesRegex(host.PilotError, "explicit account reconciliation"):
            host.cleanup(aws, value, self.path)
        self.assertFalse(any(c[1].startswith("delete") for c in aws.calls))

    def test_foreign_instance_is_not_terminated(self):
        aws = FakeAws(by_id=[instance(run_id="vroom-pilot-aaaaaaaaaaaa")])
        with self.assertRaisesRegex(host.PilotError, "ownership differs"):
            host.cleanup(aws, manifest(), self.path)
        self.assertFalse(any(c[1] == "terminate-instances" for c in aws.calls))

    def test_attached_volume_retains_guard(self):
        volume = {"VolumeId": "vol-owned", "State": "in-use", "Attachments": [{}], "Tags": host.tags(RUN)}
        aws = FakeAws(by_id=[instance("terminated")], volumes=[volume])
        with self.assertRaisesRegex(host.PilotError, "not safely detached"):
            host.cleanup(aws, manifest(), self.path)
        self.assertFalse(any(c[1].startswith("delete") for c in aws.calls))

    def test_guard_removed_only_after_positive_terminal_and_no_volume(self):
        aws = FakeAws(by_id=[instance("terminated")])
        with patch.object(host, "cleanup_control_plane") as remove:
            result = host.cleanup(aws, manifest(), self.path)
        remove.assert_called_once()
        self.assertEqual(result["status"], "cleaned")

    def test_wrong_account_stops_before_any_ec2_call(self):
        value = manifest()
        value["account"] = "999999999999"
        aws = FakeAws()
        with self.assertRaisesRegex(host.PilotError, "account differs"):
            host.cleanup(aws, value, self.path)
        self.assertEqual(len(aws.calls), 1)


class ReaperTests(unittest.TestCase):
    def test_deadline_and_verify_mode_never_terminate_early(self):
        for when, verify in ((NOW - timedelta(seconds=1), False), (NOW, True)):
            ec2 = FakeEc2([instance()])
            result = reaper.reap(ec2, RUN, NOW, when, verify=verify)
            self.assertFalse(result["termination_requested"])
            self.assertEqual(ec2.calls, [])

    def test_expiry_terminates_only_owned_returned_ids(self):
        ec2 = FakeEc2([instance()])
        self.assertTrue(reaper.reap(ec2, RUN, NOW, NOW)["termination_requested"])
        self.assertEqual(ec2.calls, [{"InstanceIds": ["i-owned"]}])

    def test_foreign_tag_even_in_filtered_response_is_rejected(self):
        ec2 = FakeEc2([instance(run_id="vroom-pilot-aaaaaaaaaaaa")])
        with self.assertRaises(ValueError):
            reaper.reap(ec2, RUN, NOW, NOW)
        self.assertEqual(ec2.calls, [])

    def test_effective_permission_requires_dry_run_operation(self):
        ec2 = FakeEc2([instance()])
        result = reaper.reap(ec2, RUN, NOW, NOW, permission_check=True)
        self.assertTrue(result["termination_permission_verified"])
        self.assertFalse(result["termination_requested"])
        self.assertEqual(ec2.calls, [{"InstanceIds": ["i-owned"], "DryRun": True}])
        ec2 = FakeEc2([instance()], dry_run_error="UnauthorizedOperation")
        with self.assertRaises(RuntimeError):
            reaper.reap(ec2, RUN, NOW, NOW, permission_check=True)

    def test_zero_matches_cannot_prove_termination_permission(self):
        result = reaper.reap(FakeEc2([]), RUN, NOW, NOW, permission_check=True)
        self.assertFalse(result["termination_permission_verified"])


class LaunchGuardTests(unittest.TestCase):
    def test_cli_cannot_retry_ambiguous_runinstances_transport(self):
        import subprocess
        result = subprocess.CompletedProcess([], 0, '{}', '')
        with patch.dict(host.os.environ, {"AWS_MAX_ATTEMPTS": "99"}), \
                patch.object(host.subprocess, "run", return_value=result) as invoked:
            cloud = host.Aws("test", "eu-north-1")
            cloud.call("ec2", "run-instances", {"ClientToken": RUN})
            self.assertEqual(invoked.call_args.kwargs["env"]["AWS_MAX_ATTEMPTS"], "1")
            cloud.call("ec2", "describe-instances")
            self.assertEqual(invoked.call_args.kwargs["env"]["AWS_MAX_ATTEMPTS"], "3")
        with patch.object(host.subprocess, "run", side_effect=subprocess.TimeoutExpired("aws", 90)) as invoked:
            with self.assertRaisesRegex(host.PilotError, "did not complete"):
                host.Aws("test", "eu-north-1").call("ec2", "run-instances", {"ClientToken": RUN})
            invoked.assert_called_once()

    def test_profile_propagation_retry_preserves_exact_request_and_deadline(self):
        value = manifest()
        deadline = value["deadline"]
        payload = {"ClientToken": RUN, "MaxCount": 1}
        responses = [host.PilotError("Invalid IAM Instance Profile name"), {"Instances": [instance()]}]
        with tempfile.TemporaryDirectory() as directory, patch.object(host, "instance_request", return_value=payload), \
                patch.object(host, "mutate", side_effect=responses) as mutate, patch.object(host, "verify_guard"), \
                patch.object(host.time, "sleep"):
            host.request_host(FakeAws(), value, Path(directory) / "run.json")
        self.assertEqual(len(mutate.call_args_list), 2)
        self.assertIs(mutate.call_args_list[0].args[-1], mutate.call_args_list[1].args[-1])
        self.assertEqual(value["deadline"], deadline)

    def test_other_launch_error_is_not_retried(self):
        with patch.object(host, "instance_request", return_value={}), \
                patch.object(host, "mutate", side_effect=host.PilotError("unknown timeout")) as mutate:
            with self.assertRaisesRegex(host.PilotError, "unknown timeout"):
                host.request_host(FakeAws(), manifest(), Path("unused.json"))
        mutate.assert_called_once()

    def test_lambda_custom_cli_receives_explicit_function_name(self):
        import subprocess
        result = subprocess.CompletedProcess([], 0, '{"StatusCode":200}', '')
        with patch.object(host.subprocess, "run", return_value=result) as invoked:
            host.Aws("test", "eu-north-1").call("lambda", "invoke",
                {"FunctionName": "owned-reaper", "Payload": "e30="}, outfile="/tmp/test-result.json")
        args = invoked.call_args.args[0]
        self.assertIn("--function-name", args)
        self.assertEqual(args[args.index("--function-name") + 1], "owned-reaper")
        self.assertNotIn("--cli-input-json", args)

    def test_cleaned_run_that_requested_ec2_cannot_restart(self):
        value = manifest()
        value["status"] = "cleaned"
        with self.assertRaisesRegex(host.PilotError, "manifest is not new"):
            host.launch(FakeAws(), value, Path("unused.json"))

    def test_cumulative_ledger_cannot_reset_for_new_run(self):
        with tempfile.TemporaryDirectory() as directory, patch.object(host, "STATE", Path(directory)):
            host.save(Path(directory) / "ledger.json", {"lifetime_cap_usd": "50", "reservations": {
                "old-run": {"reserved_usd": "49.80"}}})
            value = {"run_id": RUN, "specification": {"project_budget": {
                "authorized_lifetime_total_usd": "50", "spent_to_date_usd": "0", "outstanding_obligations_usd": "0"}},
                "preflight": {"proposal": {"cost_usd": {"estimated_run_with_contingency": "0.42"}}}}
            with self.assertRaisesRegex(host.PilotError, "lifetime allowance"):
                host.reserve_budget(value)

    def test_failed_effective_termination_proof_triggers_cleanup(self):
        class LaunchAws(FakeAws):
            def call(self, service, operation, payload=None, **kwargs):
                if operation == "run-instances":
                    return {"Instances": [instance()]}
                return super().call(service, operation, payload, **kwargs)
        value = manifest()
        value.update(status="prepared", run_requested=False, specification=host.cost.template(), instance_profile="test-profile", security_group_id="sg-test")
        fresh = {"account": ACCOUNT, "image_id": "ami-test", "subnet_id": "subnet-test", "root_device": "/dev/xvda"}
        with tempfile.TemporaryDirectory() as directory, patch.object(host, "preflight", return_value=fresh), \
                patch.object(host, "reserve_budget"), patch.object(host, "arm_guard"), patch.object(host, "prepare_host"), \
                patch.object(host, "verify_guard"), patch.object(host, "cleanup") as cleanup, \
                patch.object(host, "prove_termination_permission", side_effect=host.PilotError("proof failed")):
            with self.assertRaisesRegex(host.PilotError, "proof failed"):
                host.launch(LaunchAws(), value, Path(directory) / "run.json")
        cleanup.assert_called_once()
        self.assertEqual(value["instance_ids"], ["i-owned"])

    def test_root_identity_is_rejected(self):
        class RootAws:
            def call(self, *args):
                return {"Account": ACCOUNT, "Arn": f"arn:aws:iam::{ACCOUNT}:root"}
        with self.assertRaisesRegex(host.PilotError, "non-root"):
            host.identity(RootAws())

    def test_smoke_cannot_bypass_failed_launch_or_permission_proof(self):
        for status, proven in (("needs_cleanup", True), ("running", False), ("prepared", False)):
            value = manifest()
            value.update(status=status, termination_permission_verified=proven)
            aws = FakeAws()
            with self.assertRaisesRegex(host.PilotError, "proven expiry-role"):
                host.smoke(aws, value, Path("unused.json"))
            self.assertEqual(len(aws.calls), 1)


if __name__ == "__main__":
    unittest.main()

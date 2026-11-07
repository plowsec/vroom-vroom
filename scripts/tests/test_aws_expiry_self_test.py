"""Expiry separation and observation checks, with no live AWS calls."""
import copy
import importlib.util
import tempfile
import unittest
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("host_fixtures", Path(__file__).with_name("test_aws_host_pilot.py"))
FIXTURES = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(FIXTURES)
host, FakeAws, manifest, instance, NOW = (getattr(FIXTURES, n) for n in ("host", "FakeAws", "manifest", "instance", "NOW"))


class ExpirySelfTestTests(unittest.TestCase):
    def prepared(self, now=NOW, minutes=15):
        value = manifest()
        value.update(run_requested=False, specification={"runtime_minutes": 30, "root_gib": 40},
                     preflight={"image_id": "ami-test", "subnet_id": "subnet-test", "root_device": "/dev/xvda"},
                     instance_profile="test-profile", security_group_id="sg-test")
        if minutes is not None:
            value["expiry_self_test_after_minutes"] = minutes
        host.initialize_deadlines(value, now)
        return value

    def test_normal_and_legacy_manifests_keep_one_deadline(self):
        value = self.prepared(minutes=None)
        expected = NOW + timedelta(minutes=30)
        self.assertEqual(host.deadlines(value), (expected, expected))
        self.assertNotIn("host_shutdown_deadline", value)
        legacy = manifest()
        self.assertEqual(host.deadlines(legacy)[0], host.deadlines(legacy)[1])
        bootstrap = host.instance_request(value)["UserData"]
        self.assertIn(expected.strftime("%Y-%m-%d %H:%M:%S UTC"), bootstrap)

    def test_mode_keeps_original_host_timer_and_cost_specification(self):
        normal = self.prepared(minutes=None)
        value = self.prepared()
        self.assertEqual(value["specification"], normal["specification"])
        self.assertEqual(host.deadlines(value), (NOW + timedelta(minutes=15), NOW + timedelta(minutes=30)))
        self.assertEqual(host.instance_request(value), host.instance_request(normal))
        with_microseconds = self.prepared(now=NOW.replace(microsecond=987654))
        self.assertEqual(host.deadlines(with_microseconds)[1], NOW + timedelta(minutes=30))

    def test_invalid_mode_and_deadline_changes_are_rejected(self):
        for minutes in [True, 0, 4, 26, 30, "15", 15.5]:
            with self.assertRaises(host.PilotError):
                self.prepared(minutes=minutes)
        for key in ["deadline", "host_shutdown_deadline", "launch_window_started_at"]:
            value = self.prepared()
            value[key] = (host.cost.timestamp(value[key], key) + timedelta(seconds=1)).isoformat()
            with self.assertRaisesRegex(host.PilotError, "bounded launch window"):
                host.deadlines(value)
        value = self.prepared()
        del value["expiry_self_test_after_minutes"]
        with self.assertRaisesRegex(host.PilotError, "explicit expiry"):
            host.deadlines(value)

    def test_once_ec2_requested_neither_deadline_can_be_replaced(self):
        value = self.prepared()
        value["run_requested"] = True
        before = copy.deepcopy(value)
        with self.assertRaisesRegex(host.PilotError, "cannot be replaced"):
            host.initialize_deadlines(value, NOW + timedelta(minutes=1))
        self.assertEqual(value, before)

    def test_request_and_workload_use_earlier_guard_cutoff(self):
        value = self.prepared(now=datetime.now(timezone.utc) - timedelta(minutes=13))
        with patch.object(host, "mutate") as mutate:
            with self.assertRaisesRegex(host.PilotError, "too little time"):
                host.request_host(FakeAws(), value, Path("unused.json"))
        mutate.assert_not_called()
        value = self.prepared(now=datetime.now(timezone.utc) - timedelta(minutes=14))
        value["termination_permission_verified"] = True
        aws = FakeAws(by_tags=[instance()])
        with patch.object(host, "verify_guard"), patch.object(host, "mutate") as mutate:
            with self.assertRaisesRegex(host.PilotError, "too little time"):
                host.smoke(aws, value, Path("unused.json"))
        mutate.assert_not_called()
        self.assertFalse(any(c[0] == "ssm" for c in aws.calls))

    def test_exact_request_retries_preserve_both_cutoffs(self):
        value = self.prepared(now=datetime.now(timezone.utc))
        before = host.deadlines(value)
        with tempfile.TemporaryDirectory() as directory, patch.object(host, "verify_guard"), \
                patch.object(host.time, "sleep"), patch.object(host, "mutate", side_effect=[
                    host.PilotError("Invalid IAM Instance Profile name"), {"Instances": [instance()]}]) as mutate:
            host.request_host(FakeAws(), value, Path(directory) / "run.json")
        self.assertEqual(host.deadlines(value), before)
        self.assertIs(mutate.call_args_list[0].args[-1], mutate.call_args_list[1].args[-1])

    def observe(self, value, now, aws=None):
        aws = aws or FakeAws(by_id=[instance("terminated")])
        with tempfile.TemporaryDirectory() as directory, patch.object(host, "verify_guard"), patch.object(host, "datetime") as clock:
            clock.now.return_value = now
            result = host.expiry_status(aws, value, Path(directory) / "run.json")
        self.assertTrue(all(c[1] in ("get-caller-identity", "describe-instances") for c in aws.calls))
        return result

    def test_termination_before_guard_never_becomes_a_success_later(self):
        value = self.prepared()
        first = self.observe(value, NOW + timedelta(minutes=14))
        later = self.observe(value, NOW + timedelta(minutes=16))
        self.assertEqual(first["first_terminal_observation"], later["first_terminal_observation"])
        self.assertEqual(later["observation_status"], "terminated_before_guard_deadline")
        self.assertFalse(later["terminal_observed_between_deadlines"])
        self.assertFalse(later["self_test_passed"])

    def test_timely_terminal_observation_is_not_guard_attribution(self):
        result = self.observe(self.prepared(), NOW + timedelta(minutes=16))
        self.assertTrue(result["terminal_observed_between_deadlines"])
        self.assertEqual(result["scheduled_guard_attribution"], "unverified")
        self.assertFalse(result["self_test_passed"])
        late = self.observe(self.prepared(), NOW + timedelta(minutes=30))
        self.assertFalse(late["terminal_observed_between_deadlines"])

    def test_exact_id_absence_foreign_ownership_and_ambiguity_are_rejected(self):
        for aws in [FakeAws(), FakeAws(by_id=[instance(run_id="vroom-pilot-aaaaaaaaaaaa")])]:
            with self.assertRaisesRegex(host.PilotError, "positively observed"):
                self.observe(self.prepared(), NOW + timedelta(minutes=16), aws)
        value = self.prepared()
        value["launch_response_received"] = False
        with self.assertRaisesRegex(host.PilotError, "confirmed launch"):
            self.observe(value, NOW + timedelta(minutes=16))

    def test_ambiguous_cleanup_keeps_full_host_cutoff_window(self):
        value = self.prepared(now=datetime.now(timezone.utc) - timedelta(minutes=20))
        value.update(run_requested=True, launch_response_received=False)
        aws = FakeAws(by_id=[instance("terminated")])
        with tempfile.TemporaryDirectory() as directory, patch.object(host, "cleanup_control_plane") as remove:
            with self.assertRaisesRegex(host.PilotError, "retaining guard"):
                host.cleanup(aws, value, Path(directory) / "run.json")
        remove.assert_not_called()


if __name__ == "__main__":
    unittest.main()

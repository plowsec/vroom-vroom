"""Safety tests for offline AWS proposals; no credentials or AWS calls."""
import importlib.util
from datetime import datetime, timedelta, timezone
from pathlib import Path
import unittest

loader = importlib.util.spec_from_file_location("aws_pilot", Path(__file__).parents[1] / "aws-pilot.py")
pilot = importlib.util.module_from_spec(loader)
loader.loader.exec_module(pilot)
NOW = datetime(2026, 11, 7, 12, tzinfo=timezone.utc)


def example():
    """Synthetic rates verify arithmetic, not an actual AWS quotation."""
    result = pilot.template(NOW)
    result.update(region="eu-north-1", instance_type="m7i-flex.large")
    result["project_budget"].update(spent_to_date_usd="0", outstanding_obligations_usd="0",
                                    verified_at=NOW.isoformat(), evidence_note="test ledger only")
    result["quote"].update(region="eu-north-1", instance_type="m7i-flex.large",
                           source_urls=["https://aws.amazon.com/ec2/pricing/on-demand/"])
    result["quote"]["rates_usd"] = dict(compute_per_hour="0.10", gp3_gib_per_month="0.08",
        ipv4_per_hour="0.005", egress_per_gib="0.10", control_plane_fixed="0.01")
    return result


class PilotTests(unittest.TestCase):
    def rejected(self, spec, text):
        with self.assertRaisesRegex(pilot.PlanError, text):
            pilot.prepare(spec, NOW)

    def test_proposal_never_enables_launch(self):
        output = pilot.prepare(example(), NOW)
        self.assertFalse(output["launch_enabled"])
        self.assertFalse(output["cost_usd"]["is_billing_cap"])
        self.assertEqual(output["cost_usd"]["estimated_run_with_contingency"], "0.42")
        self.assertEqual(output["cost_usd"]["remaining_after_run"], "49.58")
        self.assertTrue(output["required_storage"]["encrypted"])
        self.assertTrue(output["required_storage"]["delete_on_termination"])

    def test_unknown_spend_blocks_proposal(self):
        spec = example()
        spec["project_budget"]["spent_to_date_usd"] = None
        self.rejected(spec, "spent_to_date")

    def test_outstanding_obligations_reduce_lifetime_budget(self):
        spec = example()
        spec["project_budget"].update(spent_to_date_usd="48", outstanding_obligations_usd="1.60")
        self.rejected(spec, "remaining project allowance")

    def test_lifetime_authorization_cannot_be_raised(self):
        spec = example()
        spec["project_budget"]["authorized_lifetime_total_usd"] = "50.01"
        self.rejected(spec, "US.50")

    def test_stale_or_future_ledger_and_quote_rejected(self):
        for key, field, delta in (("project_budget", "verified_at", -61),
                                  ("quote", "checked_at", -1441),
                                  ("project_budget", "verified_at", 1),
                                  ("quote", "checked_at", 1)):
            with self.subTest(key=key, delta=delta):
                spec = example()
                spec[key][field] = (NOW + timedelta(minutes=delta)).isoformat()
                self.rejected(spec, "within|future")

    def test_no_nan_infinity_negative_or_extreme_money(self):
        for value in ("NaN", "Infinity", "-1", "1e9999", True):
            with self.subTest(value=value):
                spec = example()
                spec["quote"]["rates_usd"]["compute_per_hour"] = value
                self.rejected(spec, "compute_per_hour")

    def test_bounds_cannot_be_bypassed(self):
        for key, value in (("instance_count", 2), ("runtime_minutes", 121),
                           ("root_gib", 161), ("runtime_minutes", True),
                           ("public_ipv4_count", 2), ("contingency_percent", "0"),
                           ("extra_billable_hours_reserve", "0")):
            with self.subTest(key=key):
                spec = example()
                spec[key] = value
                self.rejected(spec, key)

    def test_quote_wrong_region_and_spoofed_source_rejected(self):
        spec = example()
        spec["quote"]["region"] = "us-east-1"
        self.rejected(spec, "must match")
        for url in ("https://aws.amazon.com.evil.example/", "https://aws.amazon.com@evil.example/",
                    "https://not-official.s3.amazonaws.com/pricing.json"):
            spec = example()
            spec["quote"]["source_urls"] = [url]
            self.rejected(spec, "official AWS")

    def test_windows_shared_host_and_arm_nested_rejected(self):
        spec = example()
        spec["guest_os"] = "windows11"
        self.rejected(spec, "Windows 11")
        spec = example()
        spec["architecture"] = "arm64"
        self.rejected(spec, "ARM64 nested")

    def test_public_ipv4_and_egress_not_assumed_free(self):
        for rate in ("ipv4_per_hour", "egress_per_gib"):
            spec = example()
            spec["quote"]["rates_usd"][rate] = "0"
            self.rejected(spec, "IPv4|egress")


if __name__ == "__main__":
    unittest.main()

#!/usr/bin/env python3
"""Cost an offline AWS pilot proposal. Never calls AWS or provisions resources."""
import argparse
from datetime import datetime, timedelta, timezone
from decimal import Decimal, InvalidOperation, ROUND_CEILING
import json
from pathlib import Path
import re
import sys
import uuid

UTC = timezone.utc
MAX_PROJECT_CAP = Decimal("50")
RATE_NAMES = ("compute_per_hour", "gp3_gib_per_month", "ipv4_per_hour",
              "egress_per_gib", "control_plane_fixed")


class PlanError(ValueError):
    pass


def timestamp(value, name):
    try:
        result = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except (ValueError, TypeError, AttributeError):
        raise PlanError(f"{name} requires an ISO-8601 timestamp") from None
    if result.tzinfo is None:
        raise PlanError(f"{name} must include a timezone")
    return result.astimezone(UTC)


def amount(value, name, positive=False):
    if isinstance(value, bool) or value is None:
        raise PlanError(f"{name} requires a decimal amount")
    try:
        result = Decimal(str(value))
    except InvalidOperation:
        raise PlanError(f"{name} requires a decimal amount") from None
    if not result.is_finite() or result < 0 or result > 1_000_000 or (positive and result == 0):
        raise PlanError(f"{name} must be finite and {'positive' if positive else 'nonnegative'}")
    return result


def integer(value, name, minimum, maximum):
    if type(value) is not int or not minimum <= value <= maximum:
        raise PlanError(f"{name} must be an integer from {minimum} to {maximum}")
    return value


def money(value):
    return str(value.quantize(Decimal("0.01"), rounding=ROUND_CEILING))


def template(now=None):
    now = now or datetime.now(UTC)
    return {
        "schema_version": 1, "run_id": "vroom-pilot-" + uuid.uuid4().hex[:12],
        "region": None, "instance_type": None, "architecture": "x86_64",
        "guest_os": "linux", "host_mode": "nested", "instance_count": 1,
        "runtime_minutes": 30, "root_gib": 40, "public_ipv4_count": 1,
        "estimated_egress_gib": "1", "extra_billable_hours_reserve": "1",
        "contingency_percent": "50",
        "project_budget": {
            "authorized_lifetime_total_usd": "50", "spent_to_date_usd": None,
            "outstanding_obligations_usd": None, "verified_at": None, "evidence_note": None,
        },
        "quote": {
            "region": None, "instance_type": None, "checked_at": now.isoformat(),
            "purchase_option": "on-demand", "host_os": "linux",
            "rates_usd": {name: None for name in RATE_NAMES}, "source_urls": [],
        },
    }


def prepare(spec, now=None):
    """Reject incomplete inputs; return a non-executable proposal."""
    now = now or datetime.now(UTC)
    if not isinstance(spec, dict) or type(spec.get("schema_version")) is not int or spec["schema_version"] != 1:
        raise PlanError("schema_version must be 1")
    run_id = spec.get("run_id", "")
    if not isinstance(run_id, str) or not re.fullmatch(r"vroom-pilot-[a-f0-9]{12}", run_id):
        raise PlanError("run_id requires a template-generated vroom-pilot ID")
    region = spec.get("region", "")
    if not isinstance(region, str) or not re.fullmatch(r"[a-z]{2}(?:-[a-z]+)+-\d+", region):
        raise PlanError("a concrete AWS region is required")
    instance_type = spec.get("instance_type", "")
    if not isinstance(instance_type, str) or not re.fullmatch(r"[a-z0-9-]+\.[a-z0-9-]+", instance_type):
        raise PlanError("a concrete EC2 instance_type is required")
    arch, guest, mode = (spec.get(k) for k in ("architecture", "guest_os", "host_mode"))
    if arch not in ("x86_64", "arm64") or guest not in ("linux", "windows11"):
        raise PlanError("supported architectures are x86_64/arm64; guests are linux/windows11")
    if mode not in ("nested", "metal"):
        raise PlanError("host_mode must be nested or metal")
    if (mode == "metal") != instance_type.split(".", 1)[1].startswith("metal"):
        raise PlanError("host_mode and instance_type must agree about bare metal")
    if mode == "nested" and arch != "x86_64":
        raise PlanError("ARM64 nested KVM is not a verified AWS pilot option")
    if guest == "windows11" and mode != "metal":
        raise PlanError("Windows 11 requires bare metal plus license review in this pilot")
    integer(spec.get("instance_count"), "instance_count", 1, 1)
    runtime = integer(spec.get("runtime_minutes"), "runtime_minutes", 15, 120)
    root_gib = integer(spec.get("root_gib"), "root_gib", 20, 160)
    ipv4_count = integer(spec.get("public_ipv4_count"), "public_ipv4_count", 0, 1)
    egress = amount(spec.get("estimated_egress_gib"), "estimated_egress_gib")
    if egress > 10:
        raise PlanError("estimated_egress_gib must not exceed 10 for a pilot run")
    reserve = amount(spec.get("extra_billable_hours_reserve"), "extra_billable_hours_reserve")
    if not Decimal("1") <= reserve <= Decimal("4"):
        raise PlanError("extra_billable_hours_reserve must be between 1 and 4")
    contingency = amount(spec.get("contingency_percent"), "contingency_percent")
    if not Decimal("25") <= contingency <= Decimal("200"):
        raise PlanError("contingency_percent must be between 25 and 200")
    budget = spec.get("project_budget")
    if not isinstance(budget, dict):
        raise PlanError("project_budget is required")
    cap = amount(budget.get("authorized_lifetime_total_usd"), "lifetime budget", positive=True)
    if cap > MAX_PROJECT_CAP:
        raise PlanError("project budget exceeds the user's US$50 lifetime authorization")
    spent = amount(budget.get("spent_to_date_usd"), "spent_to_date_usd")
    outstanding = amount(budget.get("outstanding_obligations_usd"), "outstanding_obligations_usd")
    verified = timestamp(budget.get("verified_at"), "project_budget.verified_at")
    if verified > now or now - verified > timedelta(hours=1):
        raise PlanError("spend and outstanding obligations must be verified within the last hour")
    if not isinstance(budget.get("evidence_note"), str) or not budget["evidence_note"].strip():
        raise PlanError("budget evidence_note is required; account billing alone may be delayed")
    quote = spec.get("quote")
    if not isinstance(quote, dict):
        raise PlanError("quote is required")
    if quote.get("region") != region or quote.get("instance_type") != instance_type:
        raise PlanError("quote region and instance_type must match the proposal")
    if quote.get("purchase_option") != "on-demand" or quote.get("host_os") != "linux":
        raise PlanError("initial quote must be on-demand for a Linux host")
    checked = timestamp(quote.get("checked_at"), "quote.checked_at")
    if checked > now or now - checked > timedelta(hours=24):
        raise PlanError("quote must be checked within the last 24 hours, not in the future")
    sources = quote.get("source_urls")
    if not isinstance(sources, list) or not sources or any(
        not isinstance(url, str) or not re.match(
            r"https://(?:aws\.amazon\.com|docs\.aws\.amazon\.com|b0\.p\.awsstatic\.com|pricing\.us-east-1\.amazonaws\.com)/", url)
        for url in sources
    ):
        raise PlanError("quote source_urls must contain official AWS HTTPS sources")
    raw_rates = quote.get("rates_usd")
    if not isinstance(raw_rates, dict):
        raise PlanError("quote.rates_usd is required")
    rates = {name: amount(raw_rates.get(name), name, positive=name in
                         ("compute_per_hour", "gp3_gib_per_month")) for name in RATE_NAMES}
    if ipv4_count and rates["ipv4_per_hour"] == 0:
        raise PlanError("public IPv4 requires a positive quoted hourly rate")
    if egress and rates["egress_per_gib"] == 0:
        raise PlanError("quote paid egress; do not rely on free-tier allowances")
    return assemble(spec, now, checked, verified, cap, spent, outstanding,
                    rates, runtime, root_gib, ipv4_count, egress, reserve, contingency)


def assemble(spec, now, checked, verified, cap, spent, outstanding,
             rates, runtime, root_gib, ipv4_count, egress, reserve, contingency):
    hours = Decimal(runtime) / 60 + reserve
    # 28 days conservatively overestimates prorating for longer calendar months.
    components = {
        "compute": hours * rates["compute_per_hour"],
        "root_gp3": hours * root_gib * rates["gp3_gib_per_month"] / (28 * 24),
        "public_ipv4": hours * ipv4_count * rates["ipv4_per_hour"],
        "egress_allowance": egress * rates["egress_per_gib"],
        "control_plane": rates["control_plane_fixed"],
    }
    subtotal = sum(components.values())
    estimate = Decimal(money(subtotal * (1 + contingency / 100)))
    remaining = cap - spent - outstanding
    if estimate > remaining:
        raise PlanError(f"estimated run ${money(estimate)} exceeds remaining project allowance ${money(remaining)}")
    deadline = now + timedelta(minutes=runtime)
    return {
        "kind": "offline-proposal-only", "launch_enabled": False,
        "generated_at": now.isoformat(),
        "proposal_expires_at": min(checked + timedelta(hours=24), verified + timedelta(hours=1)).isoformat(),
        "proposed_deadline_if_started_now": deadline.isoformat(),
        "specification": spec,
        "cost_usd": {
            "components_with_reserve": {key: money(value) for key, value in components.items()},
            "subtotal": money(subtotal), "estimated_run_with_contingency": money(estimate),
            "remaining_before_run": money(remaining), "remaining_after_run": money(remaining - estimate),
            "lifetime_authorization": money(cap), "is_billing_cap": False,
        },
        "required_instance_tags": {
            "Project": "vroom-pilot", "VroomRunId": spec["run_id"], "ExpiresAt": deadline.isoformat(),
        },
        "required_storage": {
            "volume_type": "gp3", "encrypted": True, "delete_on_termination": True,
            "size_gib": root_gib, "additional_volumes": 0,
        },
        "prohibited_resources": ["nat-gateway", "elastic-ip", "load-balancer", "snapshot", "ami-copy"],
        "required_before_any_launch": [
            "Verify regional type support, architecture, capacity, permissions, and current prices.",
            "Review new-account spend-limit eligibility; distinguish its monthly pretax cap from this lifetime budget.",
            "Arm and test independent AWS-side termination with exact run ownership; tags do not expire hosts.",
            "Recompute deadline at launch, including boot/setup time; use host termination timer as second guard.",
            "Review Windows licensing and media rights before Windows tests.",
            "Keep credentials and BitLocker recovery material out of logs and manifests.",
        ],
        "limitations": [
            "Offline proposal only: no resources are created, deleted, or protected by this script.",
            "Contingency reserves budget for taxes/variation but cannot enforce final charges.",
            "Egress is an estimate, not a quota. No credits or free allowances are assumed.",
            "Pricing inputs and spend evidence need independent review; AWS billing data can lag.",
            "Cost acceptance does not establish licensing, hardware availability, or guest security readiness.",
        ],
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    sub.add_parser("template", help="print an incomplete proposal template")
    plan = sub.add_parser("plan", help="validate priced input and print a disabled offline proposal")
    plan.add_argument("specification", type=Path)
    args = parser.parse_args()
    try:
        result = template() if args.command == "template" else prepare(json.loads(args.specification.read_text()))
        print(json.dumps(result, indent=2))
        return 0
    except (PlanError, OSError, json.JSONDecodeError) as error:
        print(f"aws-pilot: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())

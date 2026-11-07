"""Lambda expiry guard for exactly one tagged pilot run; no guest input."""
from datetime import datetime, timezone
import os
import re


def reap(ec2, run_id, deadline, now, verify=False, permission_check=False):
    if not re.fullmatch(r"vroom-pilot-[a-f0-9]{12}", run_id):
        raise ValueError("invalid run ID")
    if deadline.tzinfo is None:
        raise ValueError("deadline must be timezone aware")
    owned = []
    for page in ec2.get_paginator("describe_instances").paginate(Filters=[
        {"Name": "tag:Project", "Values": ["vroom-pilot"]},
        {"Name": "tag:VroomRunId", "Values": [run_id]},
        {"Name": "instance-state-name", "Values": ["pending", "running", "stopping", "stopped"]},
    ]):
        for reservation in page["Reservations"]:
            for instance in reservation["Instances"]:
                tags = {item["Key"]: item["Value"] for item in instance.get("Tags", [])}
                if tags.get("Project") != "vroom-pilot" or tags.get("VroomRunId") != run_id:
                    raise ValueError("AWS result violated exact ownership filters")
                owned.append(instance["InstanceId"])
    expired = now >= deadline
    permitted = False
    if permission_check and owned:
        try:
            ec2.terminate_instances(InstanceIds=owned, DryRun=True)
        except Exception as error:
            if getattr(error, "response", {}).get("Error", {}).get("Code") != "DryRunOperation":
                raise
            permitted = True
        else:
            raise ValueError("termination dry-run did not return DryRunOperation")
    if expired and not verify and not permission_check and owned:
        ec2.terminate_instances(InstanceIds=owned)
    return {"run_id": run_id, "deadline": deadline.isoformat(), "expired": expired,
            "owned_count": len(owned), "termination_permission_verified": permitted,
            "termination_requested": expired and not verify and not permission_check and bool(owned)}


def handler(event, context):
    import boto3
    arn = context.invoked_function_arn.split(":")
    if arn[3] != os.environ["PILOT_REGION"] or arn[4] != os.environ["PILOT_ACCOUNT"]:
        raise ValueError("Lambda account or region mismatch")
    return reap(boto3.client("ec2", region_name=os.environ["PILOT_REGION"]), os.environ["PILOT_RUN_ID"],
                datetime.fromisoformat(os.environ["PILOT_DEADLINE"]), datetime.now(timezone.utc),
                verify=event.get("verify") is True, permission_check=event.get("permission_check") is True)

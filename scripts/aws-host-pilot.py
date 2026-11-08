#!/usr/bin/env python3
"""One bounded AWS Linux KVM host pilot. Preflight is read-only; launch/cleanup mutate AWS."""
import argparse
import base64
from datetime import datetime, timedelta, timezone
from decimal import Decimal
import fcntl
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import time
import zipfile

ROOT = Path(__file__).resolve().parents[1]
STATE = ROOT / ".aws-pilot"
UTC = timezone.utc
REGION = "eu-north-1"
INSTANCE_TYPE = "m7i-flex.large"
DEFAULT_HOST_PROFILE = "x64-nested-free"
HOST_PROFILES = {
    DEFAULT_HOST_PROFILE: {"instance_type": INSTANCE_TYPE, "architecture": "x86_64", "host_mode": "nested", "account_plan": "FREE"},
    "x64-nested-paid": {"instance_type": INSTANCE_TYPE, "architecture": "x86_64", "host_mode": "nested", "account_plan": "PAID"},
    "arm64-metal-paid": {"instance_type": "c6g.metal", "architecture": "arm64", "host_mode": "metal", "account_plan": "PAID"},
}
SSM_POLICY = "arn:aws:iam::aws:policy/AmazonSSMManagedInstanceCore"
loader = importlib.util.spec_from_file_location("cost_pilot", ROOT / "scripts/aws-pilot.py")
cost = importlib.util.module_from_spec(loader)
loader.loader.exec_module(cost)


class PilotError(RuntimeError):
    pass


def host_profile(name=DEFAULT_HOST_PROFILE):
    if not isinstance(name, str) or name not in HOST_PROFILES:
        raise PilotError("unlisted host profile; only the reviewed x64 nested and ARM64 metal profiles are allowed")
    return HOST_PROFILES[name]


def manifest_profile(manifest):
    # Old manifests retain the exact original x64 policy for status/cleanup.
    return host_profile(manifest.get("host_profile", DEFAULT_HOST_PROFILE))


class Aws:
    def __init__(self, profile, region):
        self.profile, self.region = profile, region
        self.binary = shutil.which("aws") or str(Path.home() / ".local/bin/aws")

    def call(self, service, operation, payload=None, region=None, outfile=None):
        args = [self.binary, "--profile", self.profile, "--region", region or self.region,
                "--no-cli-pager", "--no-cli-auto-prompt", "--cli-connect-timeout", "10",
                "--cli-read-timeout", "30", "--cli-binary-format", "base64", "--output", "json", service, operation]
        if service == "lambda" and operation == "invoke":
            # Lambda's custom CLI command validates these before loading cli-input-json.
            args += ["--function-name", payload["FunctionName"], "--payload", payload["Payload"]]
        elif payload:
            args += ["--cli-input-json", json.dumps(payload)]
        if outfile:
            args.append(str(outfile))
        # Only request_host may retry RunInstances, after a definite IAM-profile
        # rejection. CLI transport retries would hide ambiguous launch attempts.
        attempts = "1" if (service, operation) == ("ec2", "run-instances") else "3"
        env = dict(os.environ, AWS_PAGER="", AWS_MAX_ATTEMPTS=attempts, AWS_IGNORE_CONFIGURED_ENDPOINT_URLS="true")
        for name in env:
            if name.startswith("AWS_ENDPOINT_URL"):
                raise PilotError("custom AWS endpoints are not permitted for this pilot")
        try:
            result = subprocess.run(args, capture_output=True, text=True, timeout=90, env=env)
        except (OSError, subprocess.TimeoutExpired) as exc:
            raise PilotError(f"AWS {service} {operation} did not complete; inspect saved run state") from exc
        if result.returncode:
            detail = re.sub(r"\b\d{12}\b", "[account]", result.stderr.strip())
            raise PilotError(f"AWS {service} {operation}: {detail}")
        return json.loads(result.stdout) if result.stdout.strip() else {}


def save(path, data):
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    temporary = path.with_suffix(".tmp")
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w") as stream:
        json.dump(data, stream, indent=2)
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def read(path):
    return json.loads(path.read_text())


def self_test_minutes(specification, minutes):
    """Opt-in changes the guard only; reserve the full original host runtime."""
    if minutes is None:
        return None
    runtime = specification["runtime_minutes"]
    if (type(minutes) is not int or type(runtime) is not int
            or not 15 <= runtime <= 30 or not 5 <= minutes <= runtime - 5):
        raise PilotError("expiry self-test needs an integer cutoff >=5 minutes and >=5 minutes before the original host cutoff")
    return minutes


def initialize_deadlines(manifest, now):
    if manifest.get("run_requested"):
        raise PilotError("an EC2 request already exists; its deadlines cannot be replaced")
    minutes = self_test_minutes(manifest["specification"], manifest.get("expiry_self_test_after_minutes"))
    if minutes is not None:
        # systemd's emitted timer has second precision. Round down, never
        # extend the original host allowance, and compare observations to it.
        now = now.replace(microsecond=0)
    fallback = now + timedelta(minutes=manifest["specification"]["runtime_minutes"])
    manifest["deadline"] = (now + timedelta(minutes=minutes) if minutes is not None else fallback).isoformat()
    if minutes is not None:
        manifest["launch_window_started_at"] = now.isoformat()
        manifest["host_shutdown_deadline"] = fallback.isoformat()


def deadlines(manifest):
    """Return (guard/workload cutoff, host fallback); old manifests have one."""
    guard = cost.timestamp(manifest["deadline"], "deadline")
    fallback = cost.timestamp(manifest.get("host_shutdown_deadline", manifest["deadline"]), "host shutdown deadline")
    minutes = manifest.get("expiry_self_test_after_minutes")
    if minutes is None:
        if fallback != guard:
            raise PilotError("a different host deadline requires the explicit expiry self-test mode")
    else:
        self_test_minutes(manifest["specification"], minutes)
        started = cost.timestamp(manifest["launch_window_started_at"], "launch window start")
        if (guard != started + timedelta(minutes=minutes)
                or fallback != started + timedelta(minutes=manifest["specification"]["runtime_minutes"])):
            raise PilotError("expiry self-test deadlines differ from the original bounded launch window")
    return guard, fallback


def tags(run_id):
    return [{"Key": "Project", "Value": "vroom-pilot"},
            {"Key": "VroomRunId", "Value": run_id}]


def owns(resource_tags, run_id):
    values = {tag["Key"]: tag["Value"] for tag in resource_tags}
    return values.get("Project") == "vroom-pilot" and values.get("VroomRunId") == run_id


def identity(aws, expected=None):
    who = aws.call("sts", "get-caller-identity")
    if not re.fullmatch(r"\d{12}", who.get("Account", "")) or who.get("Arn", "").endswith(":root"):
        raise PilotError("a non-root AWS session is required")
    if expected and who["Account"] != expected:
        raise PilotError("AWS account differs from the saved manifest")
    return who["Account"]


def host_instances(aws, run_id=None):
    filters = [{"Name": "tag:Project", "Values": ["vroom-pilot"]}]
    if run_id:
        filters.append({"Name": "tag:VroomRunId", "Values": [run_id]})
    result = aws.call("ec2", "describe-instances", {"Filters": filters})
    return [instance for reservation in result["Reservations"] for instance in reservation["Instances"]]


def preflight(aws, spec, profile_name=DEFAULT_HOST_PROFILE):
    profile = host_profile(profile_name)
    proposal = cost.prepare(spec)
    if spec["region"] != REGION or spec["instance_type"] != profile["instance_type"]:
        raise PilotError(f"host profile is limited to {REGION} {profile['instance_type']}")
    if (spec["guest_os"], spec["architecture"], spec["host_mode"]) != ("linux", profile["architecture"], profile["host_mode"]):
        raise PilotError("guest OS, architecture or host mode differs from the reviewed host profile")
    if spec["runtime_minutes"] > 30 or spec["root_gib"] > 40 or spec["public_ipv4_count"] != 1:
        raise PilotError("first run requires <=30 minutes, <=40GiB, exactly one ephemeral IPv4")
    if profile["host_mode"] == "metal" and spec["root_gib"] != 40:
        raise PilotError("ARM64 metal profile requires exactly 40GiB of root storage")
    account = identity(aws)
    plan = aws.call("freetier", "get-account-plan-state", region="us-east-1")
    credit = plan.get("accountPlanRemainingCredits", {})
    if plan.get("accountId") != account or plan.get("accountPlanType") != profile["account_plan"] or plan.get("accountPlanStatus") != "ACTIVE":
        raise PilotError(f"an active {profile['account_plan']} Plan on the same account is required; no automatic upgrade")
    if profile["account_plan"] == "FREE":
        if credit.get("unit") != "USD" or cost.amount(credit.get("amount"), "remaining credits") < Decimal(
                proposal["cost_usd"]["estimated_run_with_contingency"]):
            raise PilotError("Free Plan credits do not cover the conservative run estimate")
        if cost.timestamp(plan.get("accountPlanExpirationDate"), "plan expiration") < datetime.now(UTC) + timedelta(hours=2):
            raise PilotError("Free Plan expires too soon for this bounded run")
    if any(item["State"]["Name"] != "terminated" for item in host_instances(aws)):
        raise PilotError("an existing project instance must be reconciled before another launch")
    types = aws.call("ec2", "describe-instance-types", {"InstanceTypes": [profile["instance_type"]]})["InstanceTypes"]
    if len(types) != 1:
        raise PilotError("instance type could not be verified")
    info = types[0]
    if profile["host_mode"] == "nested":
        if profile_name == "x64-nested-paid" and (
                info.get("InstanceType") != profile["instance_type"] or info.get("BareMetal") is not False
                or info.get("ProcessorInfo", {}).get("SupportedArchitectures") != ["x86_64"]
                or type(info.get("VCpuInfo", {}).get("DefaultVCpus")) is not int
                or info["VCpuInfo"]["DefaultVCpus"] != 2):
            raise PilotError("Paid x64 metadata must prove m7i-flex.large, x86_64 and exactly two virtual vCPUs")
        if info.get("FreeTierEligible") is not True:
            raise PilotError("instance is not reported Free Tier eligible; no automatic plan upgrade")
        if "nested-virtualization" not in info.get("ProcessorInfo", {}).get("SupportedFeatures", []):
            raise PilotError("instance does not report nested virtualization support")
    elif (info.get("InstanceType") != profile["instance_type"] or info.get("BareMetal") is not True
          or info.get("ProcessorInfo", {}).get("SupportedArchitectures") != ["arm64"]
          or type(info.get("VCpuInfo", {}).get("DefaultVCpus")) is not int
          or info["VCpuInfo"]["DefaultVCpus"] != 64):
        raise PilotError("ARM64 metal instance metadata must prove c6g.metal, arm64 and exactly 64 vCPUs")
    quota = aws.call("service-quotas", "get-service-quota",
                     {"ServiceCode": "ec2", "QuotaCode": "L-1216C47A"})["Quota"]["Value"]
    if profile["host_mode"] == "metal" or profile_name == "x64-nested-paid":
        quota = cost.amount(quota, "standard vCPU quota")
    if quota < info["VCpuInfo"]["DefaultVCpus"]:
        raise PilotError("regional On-Demand standard-instance vCPU quota is insufficient")
    ami_id = aws.call("ssm", "get-parameter", {"Name":
        "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-" + profile["architecture"]})["Parameter"]["Value"]
    images = aws.call("ec2", "describe-images", {"ImageIds": [ami_id], "Owners": ["amazon"]})["Images"]
    if len(images) != 1 or images[0].get("Architecture") != profile["architecture"] or images[0].get("State") != "available":
        raise PilotError("Amazon Linux AMI ownership/architecture could not be verified")
    image = images[0]
    if image.get("RootDeviceType") != "ebs" or image.get("ProductCodes"):
        raise PilotError("AMI must use EBS without paid marketplace products")
    mappings = image.get("BlockDeviceMappings", [])
    if len(mappings) != 1 or mappings[0].get("DeviceName") != image["RootDeviceName"] or "Ebs" not in mappings[0]:
        raise PilotError("AMI must contain exactly one root EBS mapping; no unquoted extra disks")
    vpcs = aws.call("ec2", "describe-vpcs", {"Filters": [{"Name": "is-default", "Values": ["true"]}]})["Vpcs"]
    if len(vpcs) != 1:
        raise PilotError("one existing default VPC is required; no VPC will be created automatically")
    if vpcs[0].get("InstanceTenancy") != "default":
        raise PilotError("selected VPC must explicitly use default tenancy; dedicated charges are not quoted")
    vpc_id = vpcs[0]["VpcId"]
    subnets = aws.call("ec2", "describe-subnets", {"Filters": [
        {"Name": "vpc-id", "Values": [vpc_id]}, {"Name": "state", "Values": ["available"]}]})["Subnets"]
    routes = aws.call("ec2", "describe-route-tables", {"Filters": [
        {"Name": "vpc-id", "Values": [vpc_id]}]})["RouteTables"]
    offerings = aws.call("ec2", "describe-instance-type-offerings", {
        "LocationType": "availability-zone", "Filters": [{"Name": "instance-type", "Values": [profile["instance_type"]]}]
    })["InstanceTypeOfferings"]
    zones = {item["Location"] for item in offerings}
    candidates = [item for item in subnets if item["AvailabilityZone"] in zones
                  and item.get("AvailableIpAddressCount", 0) > 0 and public_subnet(item["SubnetId"], routes)]
    if not candidates:
        raise PilotError("no existing public subnet offers the selected instance type")
    subnet = sorted(candidates, key=lambda item: item["AvailabilityZone"])[0]
    return {"account": account, "proposal": proposal, "instance_info": info,
            "image_id": ami_id, "root_device": image["RootDeviceName"],
            "vpc_id": vpc_id, "vpc_tenancy": "default", "subnet_id": subnet["SubnetId"], "availability_zone": subnet["AvailabilityZone"]}


def public_subnet(subnet_id, route_tables):
    explicit = [table for table in route_tables if any(
        association.get("SubnetId") == subnet_id for association in table.get("Associations", []))]
    effective = explicit or [table for table in route_tables if any(
        association.get("Main") for association in table.get("Associations", []))]
    return len(effective) == 1 and any(
        route.get("DestinationCidrBlock") == "0.0.0.0/0" and route.get("State") == "active"
        and route.get("GatewayId", "").startswith("igw-") for route in effective[0].get("Routes", []))


def role_trust(service):
    return {"Version": "2012-10-17", "Statement": [{"Effect": "Allow",
        "Principal": {"Service": service}, "Action": "sts:AssumeRole"}]}


def reaper_policy(account, region, run_id):
    return {"Version": "2012-10-17", "Statement": [
        {"Effect": "Allow", "Action": "ec2:DescribeInstances", "Resource": "*",
         "Condition": {"StringEquals": {"aws:RequestedRegion": region}}},
        {"Effect": "Allow", "Action": "ec2:TerminateInstances",
         "Resource": f"arn:aws:ec2:{region}:{account}:instance/*",
         "Condition": {"StringEquals": {"ec2:ResourceTag/Project": "vroom-pilot",
                                         "ec2:ResourceTag/VroomRunId": run_id}}},
    ]}


def mutate(aws, manifest, path, service, operation, payload, **kwargs):
    manifest_profile(manifest)
    manifest["last_intended_mutation"] = {"service": service, "operation": operation,
                                         "at": datetime.now(UTC).isoformat()}
    save(path, manifest)
    return aws.call(service, operation, payload, **kwargs)


def arm_guard(aws, manifest, path):
    deadlines(manifest)
    run_id, account = manifest["run_id"], manifest["account"]
    name = run_id + "-reaper"
    manifest["guard_name"] = name
    manifest["guard_role"] = name
    save(path, manifest)
    mutate(aws, manifest, path, "iam", "create-role", {"RoleName": name,
        "AssumeRolePolicyDocument": json.dumps(role_trust("lambda.amazonaws.com")), "Tags": tags(run_id)})
    mutate(aws, manifest, path, "iam", "put-role-policy", {"RoleName": name, "PolicyName": "terminate-owned-run",
        "PolicyDocument": json.dumps(reaper_policy(account, aws.region, run_id))})
    package = io.BytesIO()
    with zipfile.ZipFile(package, "w", zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("reaper.py", (ROOT / "scripts/aws-pilot-reaper.py").read_bytes())
    manifest["guard_code_hash"] = base64.b64encode(hashlib.sha256(package.getvalue()).digest()).decode()
    payload = {"FunctionName": name, "Runtime": "python3.13", "Role": f"arn:aws:iam::{account}:role/{name}",
        "Handler": "reaper.handler", "Timeout": 30, "MemorySize": 128,
        "Code": {"ZipFile": base64.b64encode(package.getvalue()).decode()},
        "Environment": {"Variables": {"PILOT_RUN_ID": run_id, "PILOT_ACCOUNT": account,
            "PILOT_REGION": aws.region, "PILOT_DEADLINE": manifest["deadline"]}},
        "Tags": {tag["Key"]: tag["Value"] for tag in tags(run_id)}}
    for attempt in range(7):
        try:
            function = mutate(aws, manifest, path, "lambda", "create-function", payload)
            break
        except PilotError as exc:
            if "cannot be assumed by Lambda" not in str(exc) or attempt == 6:
                raise
            time.sleep(5)
    function_arn = function["FunctionArn"]
    manifest["guard_arn"] = function_arn
    save(path, manifest)
    for _ in range(30):
        config = aws.call("lambda", "get-function-configuration", {"FunctionName": name})
        if config.get("State") == "Active":
            break
        if config.get("State") == "Failed":
            raise PilotError("expiry Lambda failed initialization")
        time.sleep(2)
    else:
        raise PilotError("expiry Lambda did not become active")
    rule = mutate(aws, manifest, path, "events", "put-rule", {"Name": name, "ScheduleExpression": "rate(1 minute)",
        "State": "ENABLED", "Description": "Terminate only this expired vroom pilot run", "Tags": tags(run_id)})
    manifest["rule_arn"] = rule["RuleArn"]
    save(path, manifest)
    mutate(aws, manifest, path, "lambda", "add-permission", {"FunctionName": name, "StatementId": "scheduled-expiry",
        "Action": "lambda:InvokeFunction", "Principal": "events.amazonaws.com", "SourceArn": rule["RuleArn"],
        "SourceAccount": account})
    target = mutate(aws, manifest, path, "events", "put-targets", {"Rule": name,
        "Targets": [{"Id": "expiry", "Arn": function_arn}]})
    if target.get("FailedEntryCount", 0):
        raise PilotError("expiry schedule target attachment failed")
    output = STATE / (run_id + ".guard-check.json")
    invoked = mutate(aws, manifest, path, "lambda", "invoke", {"FunctionName": name,
        "Payload": base64.b64encode(b'{"verify":true}').decode()}, outfile=output)
    result = read(output)
    if invoked.get("FunctionError") or result.get("run_id") != run_id or result.get("deadline") != manifest["deadline"] or result.get("expired"):
        raise PilotError("independent expiry guard verification failed")
    manifest["guard_verified"] = True
    save(path, manifest)


def verify_guard(aws, manifest):
    deadlines(manifest)
    name = manifest["guard_name"]
    config = aws.call("lambda", "get-function-configuration", {"FunctionName": name})
    if config.get("State") != "Active" or config.get("CodeSha256") != manifest["guard_code_hash"]:
        raise PilotError("expiry function state or code changed")
    expected = {"PILOT_RUN_ID": manifest["run_id"], "PILOT_ACCOUNT": manifest["account"],
                "PILOT_REGION": aws.region, "PILOT_DEADLINE": manifest["deadline"]}
    if config.get("Environment", {}).get("Variables") != expected:
        raise PilotError("expiry function configuration changed")
    rule = aws.call("events", "describe-rule", {"Name": name})
    targets = aws.call("events", "list-targets-by-rule", {"Rule": name})["Targets"]
    if rule.get("State") != "ENABLED" or rule.get("ScheduleExpression") != "rate(1 minute)":
        raise PilotError("expiry schedule is not enabled")
    if len(targets) != 1 or targets[0].get("Id") != "expiry" or targets[0].get("Arn") != manifest["guard_arn"]:
        raise PilotError("expiry schedule target differs")
    policy = aws.call("iam", "get-role-policy", {"RoleName": manifest["guard_role"], "PolicyName": "terminate-owned-run"})
    if policy["PolicyDocument"] != reaper_policy(manifest["account"], aws.region, manifest["run_id"]):
        raise PilotError("expiry permissions differ from reviewed policy")
    invoke_policy = json.loads(aws.call("lambda", "get-policy", {"FunctionName": name})["Policy"])
    statements = [item for item in invoke_policy.get("Statement", []) if item.get("Sid") == "scheduled-expiry"]
    if len(statements) != 1:
        raise PilotError("expiry scheduler invocation permission is missing")
    permission = statements[0]
    conditions = permission.get("Condition", {})
    if (permission.get("Effect") != "Allow" or permission.get("Action") != "lambda:InvokeFunction"
            or permission.get("Principal") != {"Service": "events.amazonaws.com"}
            or permission.get("Resource") != manifest["guard_arn"]
            or conditions.get("ArnLike", {}).get("AWS:SourceArn") != manifest["rule_arn"]
            or conditions.get("StringEquals", {}).get("AWS:SourceAccount") != manifest["account"]):
        raise PilotError("expiry scheduler invocation permission differs")


def prove_termination_permission(aws, manifest, path):
    output = STATE / (manifest["run_id"] + ".guard-permission.json")
    for attempt in range(5):
        invoked = mutate(aws, manifest, path, "lambda", "invoke", {"FunctionName": manifest["guard_name"],
            "Payload": base64.b64encode(b'{"permission_check":true}').decode()}, outfile=output)
        result = read(output)
        if invoked.get("FunctionError"):
            raise PilotError("expiry role could not prove termination permission; host must be cleaned up")
        if (result.get("termination_permission_verified") and result.get("owned_count") == 1
                and result.get("run_id") == manifest["run_id"] and result.get("deadline") == manifest["deadline"]
                and not result.get("expired")):
            manifest["termination_permission_verified"] = True
            save(path, manifest)
            return
        if attempt < 4:
            time.sleep(2)
    raise PilotError("expiry role permission remains unverified; host must be cleaned up")


def prepare_host(aws, manifest, path):
    run_id = manifest["run_id"]
    name = run_id + "-host"
    manifest["host_role"] = name
    manifest["instance_profile"] = name
    manifest["security_group_name"] = run_id
    save(path, manifest)
    mutate(aws, manifest, path, "iam", "create-role", {"RoleName": name,
        "AssumeRolePolicyDocument": json.dumps(role_trust("ec2.amazonaws.com")), "Tags": tags(run_id)})
    mutate(aws, manifest, path, "iam", "attach-role-policy", {"RoleName": name, "PolicyArn": SSM_POLICY})
    mutate(aws, manifest, path, "iam", "create-instance-profile", {"InstanceProfileName": name, "Tags": tags(run_id)})
    mutate(aws, manifest, path, "iam", "add-role-to-instance-profile", {"InstanceProfileName": name, "RoleName": name})
    group = mutate(aws, manifest, path, "ec2", "create-security-group", {"GroupName": run_id,
        "Description": "Vroom pilot: SSM HTTPS egress only, no inbound", "VpcId": manifest["preflight"]["vpc_id"],
        "TagSpecifications": [{"ResourceType": "security-group", "Tags": tags(run_id)}]})
    manifest["security_group_id"] = group["GroupId"]
    save(path, manifest)
    security_group = aws.call("ec2", "describe-security-groups", {"GroupIds": [group["GroupId"]]})["SecurityGroups"][0]
    if security_group.get("IpPermissions"):
        raise PilotError("new pilot security group unexpectedly has inbound permissions")
    if security_group.get("IpPermissionsEgress"):
        mutate(aws, manifest, path, "ec2", "revoke-security-group-egress", {"GroupId": group["GroupId"],
            "IpPermissions": security_group["IpPermissionsEgress"]})
    mutate(aws, manifest, path, "ec2", "authorize-security-group-egress", {"GroupId": group["GroupId"],
        "IpPermissions": [{"IpProtocol": "tcp", "FromPort": 443, "ToPort": 443,
                            "IpRanges": [{"CidrIp": "0.0.0.0/0", "Description": "SSM HTTPS endpoints"}]}]})


def instance_request(manifest):
    profile = manifest_profile(manifest)
    spec = manifest["specification"]
    run_id = manifest["run_id"]
    deadline = deadlines(manifest)[1].strftime("%Y-%m-%d %H:%M:%S UTC")
    bootstrap = "\n".join([
        "#!/bin/bash", "set -eu", "trap '/sbin/shutdown -h now' ERR",
        "systemd-run --unit=vroom-pilot-expiry --on-calendar=" + shlex.quote(deadline) + " /sbin/shutdown -h now",
        "modprobe kvm_intel" if profile["host_mode"] == "nested" else 'test "$(uname -m)" = aarch64',
        "test -c /dev/kvm", "systemctl enable --now amazon-ssm-agent",
        "mkdir -p /var/lib/vroom-pilot", "printf '%s\\n' " + shlex.quote(run_id) + " > /var/lib/vroom-pilot/run-id", "",
    ])
    request = {"ImageId": manifest["preflight"]["image_id"], "InstanceType": profile["instance_type"],
        "MinCount": 1, "MaxCount": 1, "ClientToken": run_id,
        "Placement": {"Tenancy": "default"},
        "IamInstanceProfile": {"Name": manifest["instance_profile"]},
        "InstanceInitiatedShutdownBehavior": "terminate", "DisableApiTermination": False,
        "MetadataOptions": {"HttpTokens": "required", "HttpPutResponseHopLimit": 1, "HttpEndpoint": "enabled"},
        "NetworkInterfaces": [{"DeviceIndex": 0, "SubnetId": manifest["preflight"]["subnet_id"],
            "Groups": [manifest["security_group_id"]], "AssociatePublicIpAddress": True, "DeleteOnTermination": True}],
        "BlockDeviceMappings": [{"DeviceName": manifest["preflight"]["root_device"],
            "Ebs": {"VolumeType": "gp3", "VolumeSize": spec["root_gib"], "Encrypted": True, "DeleteOnTermination": True}}],
        "TagSpecifications": [{"ResourceType": kind, "Tags": tags(run_id)} for kind in ("instance", "volume", "network-interface")],
        # EC2 RunInstances' CLI/SDK handler performs this encoding, including
        # with --cli-input-json. Some cloud-init versions decode an extra layer,
        # but relying on that is unnecessary and obscures the actual payload.
        "UserData": bootstrap,
    }
    if profile["host_mode"] == "nested":
        request["CpuOptions"] = {"NestedVirtualization": "enabled"}
    return request


def bootstrap_check(manifest):
    """Read-only host proof, to prepend to every workload command. Never arms a timer."""
    profile = manifest_profile(manifest)
    guard, fallback = deadlines(manifest)
    expected = int(fallback.timestamp())
    return "\n".join([
        "python3 -I - <<'VROOM_BOOTSTRAP_CHECK' || exit $?", "import json, os, pathlib, re, stat, subprocess, time",
        "run_id = " + repr(manifest["run_id"]), "expected = " + str(expected),
        "guard = " + str(int(guard.timestamp())),
        *(["assert os.uname().sysname == 'Linux' and os.uname().machine == 'aarch64', 'ARM64 Linux host required'"]
          if profile["host_mode"] == "metal" else []),
        "env = dict(os.environ, LC_ALL='C', TZ='UTC')",
        "def command(*args):",
        "    return subprocess.check_output(args, text=True, timeout=5, env=env).strip()",
        "def prop(unit, name):",
        "    return command('systemctl', 'show', unit, '--property=' + name, '--value')",
        "base = pathlib.Path('/var/lib/vroom-pilot')",
        "assert not base.is_symlink() and not (base / 'run-id').is_symlink(), 'unsafe bootstrap marker'",
        "assert (base / 'run-id').read_text().strip() == run_id, 'bootstrap run-id differs'",
        "assert stat.S_ISCHR(os.stat('/dev/kvm').st_mode), 'KVM device missing'",
        "timer, service = 'vroom-pilot-expiry.timer', 'vroom-pilot-expiry.service'",
        "assert prop(timer, 'LoadState') == 'loaded' and prop(timer, 'ActiveState') == 'active', 'expiry timer is not active'",
        "assert prop(timer, 'Transient') == 'yes' and prop(timer, 'Unit') == service, 'unexpected expiry timer target'",
        "next_fire = prop(timer, 'NextElapseUSecRealtime')",
        "actual = int(command('date', '--date=' + next_fire, '+%s'))",
        "assert actual == expected and time.time() < guard, 'expiry timer deadline differs or workload cutoff has passed'",
        "assert prop(service, 'LoadState') == 'loaded' and prop(service, 'Transient') == 'yes', 'expiry service missing'",
        "action = prop(service, 'ExecStart')",
        "assert re.fullmatch(r'\\{ path=/sbin/shutdown ; argv\\[\\]=/sbin/shutdown -h now ; ignore_errors=no ; [^{}]*\\}', action), 'expiry action differs'",
        "assert prop(service, 'User') in ('', 'root') and prop(service, 'Result') == 'success', 'expiry service cannot shut down host'",
        "assert time.time() < guard, 'workload cutoff passed while checking the timer'",
        "print(json.dumps({'bootstrap_verified': True, 'run_id': run_id, 'host_shutdown_epoch': actual}))",
        "VROOM_BOOTSTRAP_CHECK", "",
    ])


def reserve_budget(manifest):
    path = STATE / "ledger.json"
    ledger = read(path) if path.exists() else {"lifetime_cap_usd": "50", "reservations": {}}
    run_id = manifest["run_id"]
    if run_id in ledger["reservations"]:
        raise PilotError("run already has a reservation; reconcile rather than relaunch")
    spec = manifest["specification"]
    estimate = Decimal(manifest["preflight"]["proposal"]["cost_usd"]["estimated_run_with_contingency"])
    prior = sum(Decimal(item["reserved_usd"]) for item in ledger["reservations"].values())
    reported = sum(cost.amount(spec["project_budget"][key], key)
                   for key in ("spent_to_date_usd", "outstanding_obligations_usd"))
    cap = min(Decimal("50"), cost.amount(ledger["lifetime_cap_usd"], "ledger cap"),
              cost.amount(spec["project_budget"]["authorized_lifetime_total_usd"], "authorization"))
    if prior + reported + estimate > cap:
        raise PilotError("local reservations plus reported obligations exceed the lifetime allowance")
    ledger["reservations"][run_id] = {"reserved_usd": str(estimate), "at": datetime.now(UTC).isoformat()}
    save(path, ledger)


def request_host(aws, manifest, path):
    """Retry only a definite IAM propagation rejection, with the identical request."""
    payload = instance_request(manifest)
    for attempt in range(3):
        if datetime.now(UTC) + timedelta(minutes=3) >= deadlines(manifest)[0]:
            raise PilotError("too little time remains for the exact host request")
        try:
            return mutate(aws, manifest, path, "ec2", "run-instances", payload)
        except PilotError as error:
            if "Invalid IAM Instance Profile name" not in str(error) or attempt == 2:
                raise
            manifest.setdefault("run_retries", []).append({"at": datetime.now(UTC).isoformat(),
                "reason": "definite IAM profile propagation rejection", "client_token_unchanged": True})
            save(path, manifest)
            verify_guard(aws, manifest)
            time.sleep(5)
    raise PilotError("exact host request did not complete")


def launch(aws, manifest, path):
    manifest_profile(manifest)
    identity(aws, manifest["account"])
    retry_setup = (manifest["status"] == "cleaned" and not manifest.get("run_requested")
                   and manifest.get("setup_attempts", 1) < 2)
    if manifest["status"] != "prepared" and not retry_setup:
        raise PilotError("manifest is not new; use status/cleanup, never relaunch with a new token")
    fresh = preflight(aws, manifest["specification"], manifest.get("host_profile", DEFAULT_HOST_PROFILE))
    if fresh["account"] != manifest["account"]:
        raise PilotError("preflight account changed")
    manifest["preflight"] = fresh
    initialize_deadlines(manifest, datetime.now(UTC))
    if retry_setup:
        name = manifest["guard_name"]
        for service, operation, payload in (
            ("lambda", "get-function", {"FunctionName": name}),
            ("events", "describe-rule", {"Name": name}),
            ("iam", "get-role", {"RoleName": manifest["guard_role"]}),
        ):
            if absent_ok(aws, service, operation, payload) is not None:
                raise PilotError("previous guard resources still exist; setup retry blocked")
        prior = read(STATE / "ledger.json")["reservations"][manifest["run_id"]]
        if Decimal(prior["reserved_usd"]) < Decimal(fresh["proposal"]["cost_usd"]["estimated_run_with_contingency"]):
            raise PilotError("setup retry exceeds retained original reservation")
        manifest.setdefault("setup_retries", []).append({"at": datetime.now(UTC).isoformat(),
            "reason": "confirmed cleaned pre-EC2 setup failure", "reservation_retained": True})
    else:
        reserve_budget(manifest)
    manifest["setup_attempts"] = manifest.get("setup_attempts", 1 if retry_setup else 0) + 1
    manifest["status"] = "provisioning"
    save(path, manifest)
    try:
        arm_guard(aws, manifest, path)
        prepare_host(aws, manifest, path)
        verify_guard(aws, manifest)
        if datetime.now(UTC) + timedelta(minutes=3) >= deadlines(manifest)[0]:
            raise PilotError("too little time remains; no host will be started")
        manifest["run_requested"] = True
        manifest["status"] = "launch_requested"
        save(path, manifest)
        result = request_host(aws, manifest, path)
        manifest["instance_ids"] = [item["InstanceId"] for item in result["Instances"]]
        manifest["status"] = "running"
        save(path, manifest)
        if len(result["Instances"]) != 1 or not owns(result["Instances"][0].get("Tags", []), manifest["run_id"]):
            raise PilotError("instance response violated the exact one-host ownership contract")
        manifest["launch_response_received"] = True
        save(path, manifest)
        verify_guard(aws, manifest)
        prove_termination_permission(aws, manifest, path)
        return manifest
    except BaseException:
        manifest["status"] = "needs_cleanup"
        save(path, manifest)
        try:
            cleanup(aws, manifest, path)
        except Exception as error:
            print(f"Cleanup incomplete; expiry guard retained: {error}", file=sys.stderr)
        raise


def absent_ok(aws, service, operation, payload):
    try:
        return aws.call(service, operation, payload)
    except PilotError as error:
        if any(code in str(error) for code in ("NoSuchEntity", "ResourceNotFoundException", "InvalidGroup.NotFound", "InvalidVolume.NotFound")):
            return None
        raise


def owned_role(aws, name, run_id):
    role = absent_ok(aws, "iam", "get-role", {"RoleName": name})
    if role and not owns(role["Role"].get("Tags", []), run_id):
        raise PilotError("refusing to touch an IAM role with different ownership")
    return role


def cleanup(aws, manifest, path):
    manifest_profile(manifest)
    identity(aws, manifest["account"])
    run_id = manifest["run_id"]
    instances = host_instances(aws, run_id)
    known_ids = manifest.get("instance_ids", [])
    if known_ids:
        observed = aws.call("ec2", "describe-instances", {"InstanceIds": known_ids})
        exact = [item for reservation in observed["Reservations"] for item in reservation["Instances"]]
        if {item["InstanceId"] for item in exact} != set(known_ids):
            raise PilotError("known instance is not positively observed; expiry guard retained")
        instances = list({item["InstanceId"]: item for item in instances + exact}.values())
    if any(not owns(item.get("Tags", []), run_id) for item in instances):
        raise PilotError("refusing cleanup because instance ownership differs")
    active = [item["InstanceId"] for item in instances if item["State"]["Name"] not in ("terminated", "shutting-down")]
    if active:
        mutate(aws, manifest, path, "ec2", "terminate-instances", {"InstanceIds": active})
    if any(item["State"]["Name"] != "terminated" for item in instances):
        manifest["status"] = "terminating"
        save(path, manifest)
        raise PilotError("termination requested; rerun cleanup after instances are terminal (guard retained)")
    if manifest.get("run_requested") and not manifest.get("launch_response_received"):
        if not known_ids:
            raise PilotError("ambiguous launch without known instance IDs; guard retained pending explicit account reconciliation")
        # A self-test must not shorten the original full-lifetime ambiguity guard.
        last_cutoff = max(cost.timestamp(manifest["deadline"], "deadline"),
                          cost.timestamp(manifest.get("host_shutdown_deadline", manifest["deadline"]), "host shutdown deadline"))
        if datetime.now(UTC) < last_cutoff + timedelta(minutes=2):
            raise PilotError("ambiguous launch: retaining guard through deadline plus two minutes")
    known_volumes = set(manifest.get("volume_ids", []))
    for instance in instances:
        known_volumes.update(item["Ebs"]["VolumeId"] for item in instance.get("BlockDeviceMappings", []) if "Ebs" in item)
    manifest["volume_ids"] = sorted(known_volumes)
    save(path, manifest)
    volumes = aws.call("ec2", "describe-volumes", {"Filters": [
        {"Name": "tag:Project", "Values": ["vroom-pilot"]}, {"Name": "tag:VroomRunId", "Values": [run_id]}]})["Volumes"]
    for volume_id in known_volumes:
        direct = absent_ok(aws, "ec2", "describe-volumes", {"VolumeIds": [volume_id]})
        if direct:
            volumes.extend(direct["Volumes"])
    volumes = list({item["VolumeId"]: item for item in volumes}.values())
    if any(not owns(item.get("Tags", []), run_id) for item in volumes):
        raise PilotError("volume ownership differs; expiry guard retained")
    for volume in volumes:
        if volume.get("Attachments") or volume["State"] not in ("available", "deleting", "deleted"):
            raise PilotError("owned root volume is not safely detached; guard retained")
        if volume["State"] == "available":
            mutate(aws, manifest, path, "ec2", "delete-volume", {"VolumeId": volume["VolumeId"]})
    if volumes:
        raise PilotError("owned volume deletion pending; rerun cleanup before removing guard")
    cleanup_control_plane(aws, manifest)
    manifest["status"] = "cleaned"
    manifest["cleaned_at"] = datetime.now(UTC).isoformat()
    save(path, manifest)
    return manifest


def cleanup_control_plane(aws, manifest):
    run_id = manifest["run_id"]
    groups = aws.call("ec2", "describe-security-groups", {"Filters": [
        {"Name": "tag:Project", "Values": ["vroom-pilot"]}, {"Name": "tag:VroomRunId", "Values": [run_id]}]})["SecurityGroups"]
    for group in groups:
        if not owns(group.get("Tags", []), run_id):
            raise PilotError("security-group ownership differs")
        aws.call("ec2", "delete-security-group", {"GroupId": group["GroupId"]})
    name = manifest.get("instance_profile")
    if name:
        profile = absent_ok(aws, "iam", "get-instance-profile", {"InstanceProfileName": name})
        if profile:
            info = profile["InstanceProfile"]
            if not owns(info.get("Tags", []), run_id):
                raise PilotError("instance-profile ownership differs")
            for role in info.get("Roles", []):
                if role["RoleName"] != manifest.get("host_role"):
                    raise PilotError("instance profile contains an unexpected role")
                aws.call("iam", "remove-role-from-instance-profile", {"InstanceProfileName": name, "RoleName": role["RoleName"]})
            aws.call("iam", "delete-instance-profile", {"InstanceProfileName": name})
    host_role = manifest.get("host_role")
    if host_role and owned_role(aws, host_role, run_id):
        absent_ok(aws, "iam", "detach-role-policy", {"RoleName": host_role, "PolicyArn": SSM_POLICY})
        aws.call("iam", "delete-role", {"RoleName": host_role})
    name = manifest.get("guard_name")
    if name:
        rule = absent_ok(aws, "events", "describe-rule", {"Name": name})
        if rule:
            rule_tags = aws.call("events", "list-tags-for-resource", {"ResourceARN": rule["Arn"]})["Tags"]
            if not owns(rule_tags, run_id):
                raise PilotError("expiry-rule ownership differs")
            removed = aws.call("events", "remove-targets", {"Rule": name, "Ids": ["expiry"]})
            if removed.get("FailedEntryCount", 0):
                raise PilotError("failed removing expiry rule target")
            aws.call("events", "delete-rule", {"Name": name})
        function = absent_ok(aws, "lambda", "get-function", {"FunctionName": name})
        if function:
            if not owns([{"Key": key, "Value": value} for key, value in function.get("Tags", {}).items()], run_id):
                raise PilotError("expiry-function ownership differs")
            aws.call("lambda", "delete-function", {"FunctionName": name})
    role = manifest.get("guard_role")
    if role and owned_role(aws, role, run_id):
        absent_ok(aws, "iam", "delete-role-policy", {"RoleName": role, "PolicyName": "terminate-owned-run"})
        aws.call("iam", "delete-role", {"RoleName": role})


def smoke(aws, manifest, path):
    if manifest_profile(manifest)["host_mode"] != "nested":
        raise PilotError("this command is an x64 micro smoke; use the reviewed native ARM64 artifact workflow")
    identity(aws, manifest["account"])
    if manifest.get("status") != "running" or manifest.get("termination_permission_verified") is not True:
        raise PilotError("running status and proven expiry-role termination permission are required")
    verify_guard(aws, manifest)
    instances = host_instances(aws, manifest["run_id"])
    live = [item for item in instances if item["State"]["Name"] == "running"]
    if len(live) != 1 or not owns(live[0].get("Tags", []), manifest["run_id"]):
        raise PilotError("exactly one owned running instance is required")
    if datetime.now(UTC) + timedelta(minutes=2) >= deadlines(manifest)[0]:
        raise PilotError("too little time remains for the smoke test")
    instance_id = live[0]["InstanceId"]
    agents = aws.call("ssm", "describe-instance-information", {"Filters": [{"Key": "InstanceIds", "Values": [instance_id]}]})
    if len(agents["InstanceInformationList"]) != 1 or agents["InstanceInformationList"][0].get("PingStatus") != "Online":
        raise PilotError("SSM agent is not online yet; no fallback inbound access will be opened")
    result = mutate(aws, manifest, path, "ssm", "send-command", {
        "InstanceIds": [instance_id], "DocumentName": "AWS-RunShellScript", "Comment": manifest["run_id"],
        "TimeoutSeconds": 120, "Parameters": {"commands": ["set -eu\n" + bootstrap_check(manifest) + (ROOT / "scripts/aws-kvm-smoke.sh").read_text()],
                                                  "executionTimeout": ["120"]}})
    command_id = result["Command"]["CommandId"]
    manifest.setdefault("smoke_commands", []).append(command_id)
    save(path, manifest)
    return {"run_id": manifest["run_id"], "command_id": command_id, "instance_id": instance_id,
            "status": "submitted", "note": "Read the SSM invocation result; submission is not a passing test."}


def expiry_status(aws, manifest, path):
    """Read exact-ID termination evidence; never invoke the guard or terminate."""
    identity(aws, manifest["account"])
    if manifest.get("expiry_self_test_after_minutes") is None:
        raise PilotError("this manifest was not prepared for an expiry self-test")
    guard, fallback = deadlines(manifest)
    ids = manifest.get("instance_ids", [])
    if len(ids) != 1 or manifest.get("launch_response_received") is not True:
        raise PilotError("expiry observation requires exactly one confirmed launch instance ID")
    verify_guard(aws, manifest)
    result = aws.call("ec2", "describe-instances", {"InstanceIds": ids})
    observed = [item for reservation in result["Reservations"] for item in reservation["Instances"]]
    if len(observed) != 1 or observed[0].get("InstanceId") != ids[0] or not owns(observed[0].get("Tags", []), manifest["run_id"]):
        raise PilotError("exact owned instance is not positively observed; expiry remains unverified")
    now = datetime.now(UTC)
    state = observed[0]["State"]["Name"]
    first = manifest.get("expiry_terminal_observation")
    if state == "terminated" and first is None:
        first = {"instance_id": ids[0], "observed_at": now.isoformat()}
        manifest["expiry_terminal_observation"] = first
        save(path, manifest)
    status = "waiting_for_termination"
    timely = False
    if first is not None:
        if first.get("instance_id") != ids[0]:
            raise PilotError("terminal observation belongs to a different instance")
        when = cost.timestamp(first["observed_at"], "terminal observation")
        if when < guard:
            status = "terminated_before_guard_deadline"
        elif when >= fallback:
            status = "termination_not_observed_before_host_fallback"
        else:
            status = "termination_observed_between_deadlines"
            timely = state == "terminated"
    return {"run_id": manifest["run_id"], "instance_id": ids[0], "instance_state": state,
            "observed_at": now.isoformat(), "deadline": manifest["deadline"],
            "host_shutdown_deadline": manifest["host_shutdown_deadline"],
            "first_terminal_observation": first, "observation_status": status,
            "terminal_observed_between_deadlines": timely, "scheduled_guard_attribution": "unverified",
            "self_test_passed": False,
            "note": "Timing alone does not attribute termination. Inspect the scheduled guard's successful exact-ID CloudTrail event; do not manually invoke expiry."}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", default="vroom-pilot")
    sub = parser.add_subparsers(dest="command", required=True)
    prep = sub.add_parser("preflight", help="read AWS state and persist a disabled run manifest")
    prep.add_argument("specification", type=Path)
    prep.add_argument("--expected-account", required=True)
    prep.add_argument("--host-profile", choices=tuple(HOST_PROFILES), default=DEFAULT_HOST_PROFILE,
                      help="explicit reviewed host/account policy; never changes the account plan")
    prep.add_argument("--expiry-self-test-after-minutes", type=int,
                      help="earlier scheduled guard cutoff; original quoted runtime remains the host fallback")
    for name in ("launch", "status", "smoke", "cleanup", "expiry-status"):
        command = sub.add_parser(name)
        command.add_argument("run_id")
        if name not in ("status", "expiry-status"):
            command.add_argument("--execute-reviewed-plan", action="store_true", required=True)
    args = parser.parse_args()
    aws = Aws(args.profile, REGION)
    STATE.mkdir(mode=0o700, parents=True, exist_ok=True)
    with (STATE / "operation.lock").open("w") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        if args.command == "preflight":
            identity(aws, args.expected_account)
            spec = read(args.specification)
            minutes = self_test_minutes(spec, args.expiry_self_test_after_minutes)
            details = preflight(aws, spec, args.host_profile)
            path = STATE / (spec["run_id"] + ".json")
            if path.exists():
                raise PilotError("run manifest already exists; use a new offline specification for a new run")
            manifest = {"schema_version": 1, "run_id": spec["run_id"], "profile": args.profile,
                        "region": REGION, "account": details["account"], "status": "prepared",
                        "specification": spec, "preflight": details, "host_profile": args.host_profile}
            if minutes is not None:
                manifest["expiry_self_test_after_minutes"] = minutes
            save(path, manifest)
        else:
            if not re.fullmatch(r"vroom-pilot-[a-f0-9]{12}", args.run_id):
                raise PilotError("invalid run ID")
            path = STATE / (args.run_id + ".json")
            manifest = read(path)
            manifest_profile(manifest)
            if manifest["run_id"] != args.run_id or manifest["region"] != REGION or manifest["profile"] != args.profile:
                raise PilotError("manifest does not match selected run/profile/region")
            identity(aws, manifest["account"])
            if args.command == "launch":
                launch(aws, manifest, path)
            elif args.command == "cleanup":
                cleanup(aws, manifest, path)
            elif args.command == "smoke":
                print(json.dumps(smoke(aws, manifest, path), indent=2))
                return
            elif args.command == "expiry-status":
                print(json.dumps(expiry_status(aws, manifest, path), indent=2))
                return
            else:
                instances = host_instances(aws, manifest["run_id"])
                print(json.dumps({"run_id": manifest["run_id"], "status": manifest["status"],
                    "deadline": manifest.get("deadline"),
                    "host_shutdown_deadline": manifest.get("host_shutdown_deadline", manifest.get("deadline")),
                    "expiry_self_test_after_minutes": manifest.get("expiry_self_test_after_minutes"),
                    "instances": [{"id": item["InstanceId"],
                    "state": item["State"]["Name"]} for item in instances]}, indent=2))
                return
        print(json.dumps({"run_id": manifest["run_id"], "status": manifest["status"],
            "manifest": str(path), "deadline": manifest.get("deadline"),
            "host_shutdown_deadline": manifest.get("host_shutdown_deadline", manifest.get("deadline")),
            "expiry_self_test_after_minutes": manifest.get("expiry_self_test_after_minutes"),
            "estimated_run_usd": manifest["preflight"]["proposal"]["cost_usd"]["estimated_run_with_contingency"]}, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (PilotError, cost.PlanError, OSError, ValueError, KeyError) as error:
        print("aws-host-pilot: " + re.sub(r"\b\d{12}\b", "[account]", str(error)), file=sys.stderr)
        sys.exit(2)

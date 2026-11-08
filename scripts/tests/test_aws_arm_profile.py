"""ARM host-profile policy, using only synthetic prices and fake AWS replies."""
import copy
from datetime import datetime, timedelta, timezone
import importlib.util
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch


def fixture(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(name + '.py'))
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


base, pricing = fixture('test_aws_host_pilot'), fixture('test_aws_pilot')
host = base.host
ARM = 'arm64-metal-paid'


def specification(arm=True):
    value = pricing.example()
    now = datetime.now(timezone.utc).isoformat()
    value['project_budget']['verified_at'] = now
    value['quote']['checked_at'] = now
    if arm:
        value.update(instance_type='c6g.metal', architecture='arm64', host_mode='metal')
        value['quote']['instance_type'] = 'c6g.metal'
    return value


class PreflightAws:
    profile, region = 'test', 'eu-north-1'

    def __init__(self, arm=True):
        self.calls = []
        self.plan = {'accountId': base.ACCOUNT, 'accountPlanType': 'PAID' if arm else 'FREE',
                     'accountPlanStatus': 'ACTIVE', 'accountPlanRemainingCredits': {'unit': 'USD', 'amount': '100'},
                     'accountPlanExpirationDate': (datetime.now(timezone.utc) + timedelta(days=1)).isoformat()}
        self.info = {'InstanceType': 'c6g.metal' if arm else 'm7i-flex.large', 'BareMetal': arm,
                     'FreeTierEligible': not arm, 'VCpuInfo': {'DefaultVCpus': 64 if arm else 2},
                     'ProcessorInfo': {'SupportedArchitectures': ['arm64' if arm else 'x86_64'],
                                       'SupportedFeatures': [] if arm else ['nested-virtualization']}}
        self.quota = 64 if arm else 2
        self.vpc = {'VpcId': 'vpc-test', 'InstanceTenancy': 'default'}
        self.image = {'ImageId': 'ami-test', 'Architecture': 'arm64' if arm else 'x86_64', 'State': 'available',
                      'RootDeviceType': 'ebs', 'RootDeviceName': '/dev/xvda', 'ProductCodes': [],
                      'BlockDeviceMappings': [{'DeviceName': '/dev/xvda', 'Ebs': {'VolumeSize': 30}}]}

    def call(self, service, operation, payload=None, **_kwargs):
        self.calls.append((service, operation, payload))
        if operation == 'get-caller-identity':
            return {'Account': base.ACCOUNT, 'Arn': f'arn:aws:sts::{base.ACCOUNT}:assumed-role/role/session'}
        responses = {
            'get-account-plan-state': self.plan, 'describe-instances': {'Reservations': []},
            'describe-instance-types': {'InstanceTypes': [self.info]}, 'get-service-quota': {'Quota': {'Value': self.quota}},
            'get-parameter': {'Parameter': {'Value': 'ami-test'}}, 'describe-images': {'Images': [self.image]},
            'describe-vpcs': {'Vpcs': [self.vpc]},
            'describe-subnets': {'Subnets': [{'SubnetId': 'subnet-test', 'AvailabilityZone': 'eu-north-1a', 'AvailableIpAddressCount': 1}]},
            'describe-route-tables': {'RouteTables': [{'Associations': [{'Main': True}], 'Routes': [
                {'DestinationCidrBlock': '0.0.0.0/0', 'State': 'active', 'GatewayId': 'igw-test'}]}]},
            'describe-instance-type-offerings': {'InstanceTypeOfferings': [{'Location': 'eu-north-1a'}]},
        }
        if operation not in responses:
            raise AssertionError(f'unexpected AWS mutation: {service} {operation}')
        return copy.deepcopy(responses[operation])


def prepared(arm=True):
    aws = PreflightAws(arm)
    value = base.manifest()
    value.update(specification=specification(arm), instance_profile='test-profile', security_group_id='sg-test')
    if arm:
        value['host_profile'] = ARM
    value['preflight'] = host.preflight(aws, value['specification'], ARM if arm else host.DEFAULT_HOST_PROFILE)
    return value


class ArmProfileTests(unittest.TestCase):
    def test_arm_exact_profile_passes_read_only_preflight(self):
        aws = PreflightAws()
        result = host.preflight(aws, specification(), ARM)
        self.assertEqual(result['instance_info']['VCpuInfo']['DefaultVCpus'], 64)
        self.assertFalse(result['proposal']['launch_enabled'])
        calls = {operation: payload for _, operation, payload in aws.calls}
        self.assertEqual(calls['describe-instance-types'], {'InstanceTypes': ['c6g.metal']})
        self.assertEqual(calls['get-parameter']['Name'], '/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64')
        self.assertEqual(calls['describe-images']['Owners'], ['amazon'])
        self.assertNotIn('upgrade-account-plan', calls)

    def test_profile_is_explicit_and_unlisted_values_never_reach_aws_or_mutation(self):
        aws = PreflightAws()
        with self.assertRaises(host.PilotError):
            host.preflight(aws, specification())
        self.assertEqual(aws.calls, [])
        for name in ['c6g.metal', 'arm64', '', None, [], 'x64-paid']:
            value = dict(base.manifest(), host_profile=name)
            with self.subTest(name=name), self.assertRaises(host.PilotError):
                host.preflight(aws, specification(), name)
            with self.assertRaises(host.PilotError):
                host.mutate(aws, value, Path('unused.json'), 'ec2', 'run-instances', {})
            with self.assertRaises(host.PilotError):
                host.cleanup(aws, value, Path('unused.json'))
        self.assertEqual(aws.calls, [])

    def test_arm_rejects_wrong_type_region_mode_os_and_resource_bounds(self):
        for key, value in [('instance_type', 'c7g.metal'), ('region', 'us-east-1'), ('architecture', 'x86_64'),
                           ('host_mode', 'nested'), ('guest_os', 'windows11'), ('instance_count', 2),
                           ('runtime_minutes', 31), ('root_gib', 20), ('root_gib', 41), ('public_ipv4_count', 0)]:
            spec = specification(); spec[key] = value
            if key in ('instance_type', 'region'):
                spec['quote'][key] = value
            aws = PreflightAws()
            with self.subTest(key=key, value=value), self.assertRaises((host.PilotError, host.cost.PlanError)):
                host.preflight(aws, spec, ARM)
            self.assertEqual(aws.calls, [])

    def test_paid_active_same_account_required_without_credit_dependency(self):
        for key, value in [('accountPlanType', 'FREE'), ('accountPlanType', 'UNKNOWN'), ('accountPlanStatus', 'EXPIRED'),
                           ('accountId', '999999999999')]:
            aws = PreflightAws(); aws.plan[key] = value
            with self.subTest(key=key), self.assertRaisesRegex(host.PilotError, 'active PAID Plan'):
                host.preflight(aws, specification(), ARM)
        aws = PreflightAws()
        del aws.plan['accountPlanRemainingCredits']; del aws.plan['accountPlanExpirationDate']
        self.assertTrue(host.preflight(aws, specification(), ARM))

    def test_metal_arm64_and_64_cpu_capabilities_are_required(self):
        for key, value in [('InstanceType', 'c7g.metal'), ('BareMetal', False),
                           ('ProcessorInfo', {'SupportedArchitectures': ['x86_64']}),
                           ('VCpuInfo', {'DefaultVCpus': 32}), ('VCpuInfo', {'DefaultVCpus': 64.0})]:
            aws = PreflightAws(); aws.info[key] = value
            with self.subTest(key=key), self.assertRaisesRegex(host.PilotError, 'metadata must prove'):
                host.preflight(aws, specification(), ARM)
        aws = PreflightAws(); aws.quota = 63
        with self.assertRaisesRegex(host.PilotError, 'quota is insufficient'):
            host.preflight(aws, specification(), ARM)
        for quota in [float('nan'), float('inf'), True, -1]:
            aws = PreflightAws(); aws.quota = quota
            with self.subTest(quota=quota), self.assertRaises(host.cost.PlanError):
                host.preflight(aws, specification(), ARM)

    def test_wrong_ami_or_unquoted_extra_volume_rejected(self):
        for key, value in [('Architecture', 'x86_64'), ('State', 'pending'), ('ProductCodes', [{'ProductCodeId': 'paid'}]),
                           ('RootDeviceType', 'instance-store'), ('BlockDeviceMappings', [])]:
            aws = PreflightAws(); aws.image[key] = value
            with self.subTest(key=key), self.assertRaises(host.PilotError):
                host.preflight(aws, specification(), ARM)

    def test_arm_request_omits_nested_options_and_intel_bootstrap(self):
        value = prepared()
        request = host.instance_request(value)
        self.assertEqual(request['InstanceType'], 'c6g.metal')
        self.assertEqual((request['MinCount'], request['MaxCount']), (1, 1))
        self.assertNotIn('CpuOptions', request)
        self.assertNotIn('kvm_intel', request['UserData'])
        self.assertIn('test "$(uname -m)" = aarch64', request['UserData'])
        self.assertIn('test -c /dev/kvm', request['UserData'])
        self.assertIn(host.deadlines(value)[1].strftime('%Y-%m-%d %H:%M:%S UTC'), request['UserData'])
        self.assertEqual(request['InstanceInitiatedShutdownBehavior'], 'terminate')
        self.assertEqual(request['BlockDeviceMappings'][0]['Ebs'],
            {'VolumeType': 'gp3', 'VolumeSize': 40, 'Encrypted': True, 'DeleteOnTermination': True})
        self.assertIn("os.uname().machine == 'aarch64'", host.bootstrap_check(value))

    def test_x64_micro_smoke_rejects_arm_before_any_aws_call(self):
        aws = PreflightAws()
        with self.assertRaisesRegex(host.PilotError, 'reviewed native ARM64 artifact'):
            host.smoke(aws, dict(base.manifest(), host_profile=ARM), Path('unused.json'))
        self.assertEqual(aws.calls, [])

    def test_arm_host_verification_rejects_wrong_machine_before_work(self):
        script = host.bootstrap_check(prepared()).split('\n', 1)[1].rsplit('\nVROOM_BOOTSTRAP_CHECK', 1)[0]
        for system, machine in [('Linux', 'x86_64'), ('Darwin', 'aarch64')]:
            with self.subTest(system=system, machine=machine), \
                    patch.object(host.os, 'uname', return_value=SimpleNamespace(sysname=system, machine=machine)), \
                    patch.object(host.subprocess, 'check_output') as command:
                with self.assertRaisesRegex(AssertionError, 'ARM64 Linux host required'):
                    exec(compile(script, 'arm-bootstrap-check', 'exec'), {})
                command.assert_not_called()

    def test_legacy_x64_policy_and_explicit_default_request_are_identical(self):
        value = prepared(False)
        explicit = dict(value, host_profile=host.DEFAULT_HOST_PROFILE)
        self.assertEqual(host.instance_request(value), host.instance_request(explicit))
        self.assertEqual(host.bootstrap_check(value), host.bootstrap_check(explicit))
        request = host.instance_request(value)
        self.assertEqual(request['InstanceType'], 'm7i-flex.large')
        self.assertEqual(request['CpuOptions'], {'NestedVirtualization': 'enabled'})
        self.assertIn('modprobe kvm_intel', request['UserData'])
        aws = PreflightAws(False); aws.plan['accountPlanType'] = 'PAID'
        with self.assertRaisesRegex(host.PilotError, 'active FREE Plan'):
            host.preflight(aws, specification(False))

    def test_arm_cleanup_uses_exact_owned_id_and_retains_guard_until_terminal(self):
        value = dict(base.manifest(), host_profile=ARM)
        aws = base.FakeAws(by_id=[base.instance()])
        with tempfile.TemporaryDirectory() as directory, patch.object(host, 'cleanup_control_plane') as cleanup:
            with self.assertRaisesRegex(host.PilotError, 'guard retained'):
                host.cleanup(aws, value, Path(directory)/'run.json')
        self.assertEqual([payload for _, op, payload in aws.calls if op == 'terminate-instances'], [{'InstanceIds': ['i-owned']}])
        cleanup.assert_not_called()
        self.assertEqual(value['status'], 'terminating')

    def test_arm_reservation_keeps_lifetime_cap(self):
        value = prepared()
        with tempfile.TemporaryDirectory() as directory, patch.object(host, 'STATE', Path(directory)):
            host.save(Path(directory)/'ledger.json', {'lifetime_cap_usd': '50', 'reservations': {'prior': {'reserved_usd': '49.99'}}})
            with self.assertRaisesRegex(host.PilotError, 'lifetime allowance'):
                host.reserve_budget(value)


if __name__ == '__main__':
    unittest.main()

"""Explicit Paid x64 policy without changing the legacy Free default."""
import copy
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
import importlib.util

_spec = importlib.util.spec_from_file_location('arm_fixtures', Path(__file__).with_name('test_aws_arm_profile.py'))
fixtures = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(fixtures)
host, base = fixtures.host, fixtures.base
PAID = 'x64-nested-paid'


def aws():
    value = fixtures.PreflightAws(False)
    value.plan['accountPlanType'] = 'PAID'
    return value


def prepared():
    value = fixtures.prepared(False)
    value['host_profile'] = PAID
    value['preflight'] = host.preflight(aws(), value['specification'], PAID)
    return value


class PaidX64Tests(unittest.TestCase):
    def test_exact_paid_profile_is_read_only_and_explicit(self):
        cloud = aws()
        result = host.preflight(cloud, fixtures.specification(False), PAID)
        self.assertFalse(result['proposal']['launch_enabled'])
        calls = {operation: payload for _, operation, payload in cloud.calls}
        self.assertEqual(calls['describe-instance-types'], {'InstanceTypes': ['m7i-flex.large']})
        self.assertEqual(calls['get-parameter']['Name'], '/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-x86_64')
        self.assertEqual(calls['describe-images']['Owners'], ['amazon'])
        self.assertNotIn('upgrade-account-plan', calls)
        with self.assertRaisesRegex(host.PilotError, 'active FREE Plan'):
            host.preflight(aws(), fixtures.specification(False))

    def test_paid_active_same_account_required_without_credit_allowance(self):
        for key, value in [('accountPlanType', 'FREE'), ('accountPlanType', 'UNKNOWN'),
                           ('accountPlanStatus', 'EXPIRED'), ('accountId', '999999999999')]:
            cloud = aws(); cloud.plan[key] = value
            with self.subTest(key=key, value=value), self.assertRaisesRegex(host.PilotError, 'active PAID Plan'):
                host.preflight(cloud, fixtures.specification(False), PAID)
        cloud = aws()
        del cloud.plan['accountPlanRemainingCredits']; del cloud.plan['accountPlanExpirationDate']
        self.assertTrue(host.preflight(cloud, fixtures.specification(False), PAID))

    def test_wrong_host_or_unbounded_resources_fail_before_apis(self):
        for key, value in [('instance_type', 'm7i-flex.xlarge'), ('region', 'us-east-1'),
                           ('architecture', 'arm64'), ('host_mode', 'metal'), ('guest_os', 'windows11'),
                           ('instance_count', 2), ('runtime_minutes', 31), ('root_gib', 41), ('public_ipv4_count', 0)]:
            spec = fixtures.specification(False); spec[key] = value
            if key in ('instance_type', 'region'):
                spec['quote'][key] = value
            cloud = aws()
            with self.subTest(key=key), self.assertRaises((host.PilotError, host.cost.PlanError)):
                host.preflight(cloud, spec, PAID)
            self.assertEqual(cloud.calls, [])

    def test_exact_virtual_x64_cpu_shape_and_nested_support_required(self):
        for key, value in [('InstanceType', 'm7i-flex.xlarge'), ('BareMetal', True),
                           ('ProcessorInfo', {'SupportedArchitectures': ['arm64']}),
                           ('VCpuInfo', {'DefaultVCpus': 4}), ('VCpuInfo', {'DefaultVCpus': 2.0})]:
            cloud = aws(); cloud.info[key] = value
            with self.subTest(key=key), self.assertRaisesRegex(host.PilotError, 'metadata must prove'):
                host.preflight(cloud, fixtures.specification(False), PAID)
        cloud = aws(); cloud.info['ProcessorInfo']['SupportedFeatures'] = []
        with self.assertRaisesRegex(host.PilotError, 'nested virtualization'):
            host.preflight(cloud, fixtures.specification(False), PAID)
        cloud = aws(); cloud.info['FreeTierEligible'] = False
        with self.assertRaisesRegex(host.PilotError, 'Free Tier eligible'):
            host.preflight(cloud, fixtures.specification(False), PAID)

    def test_quota_must_be_finite_sufficient_number(self):
        for quota in [float('nan'), float('inf'), True, -1, 1]:
            cloud = aws(); cloud.quota = quota
            with self.subTest(quota=quota), self.assertRaises((host.PilotError, host.cost.PlanError)):
                host.preflight(cloud, fixtures.specification(False), PAID)

    def test_ami_platform_and_single_unpaid_root_stay_required(self):
        for key, value in [('Architecture', 'arm64'), ('State', 'pending'),
                           ('ProductCodes', [{'ProductCodeId': 'paid'}]), ('RootDeviceType', 'instance-store'),
                           ('BlockDeviceMappings', [])]:
            cloud = aws(); cloud.image[key] = value
            with self.subTest(key=key), self.assertRaises(host.PilotError):
                host.preflight(cloud, fixtures.specification(False), PAID)

    def test_paid_and_free_requests_guards_and_bootstrap_are_identical(self):
        value = prepared(); legacy = copy.deepcopy(value); legacy.pop('host_profile')
        self.assertEqual(host.instance_request(value), host.instance_request(legacy))
        self.assertEqual(host.bootstrap_check(value), host.bootstrap_check(legacy))
        request = host.instance_request(value)
        self.assertEqual((request['MinCount'], request['MaxCount']), (1, 1))
        self.assertEqual(request['CpuOptions'], {'NestedVirtualization': 'enabled'})
        self.assertEqual(request['BlockDeviceMappings'][0]['Ebs'],
                         {'VolumeType': 'gp3', 'VolumeSize': 40, 'Encrypted': True, 'DeleteOnTermination': True})
        self.assertEqual(request['InstanceInitiatedShutdownBehavior'], 'terminate')
        self.assertEqual(request['MetadataOptions']['HttpTokens'], 'required')
        self.assertEqual(request['Placement'], {'Tenancy': 'default'})

    def test_dedicated_unknown_and_missing_vpc_tenancy_fail_closed(self):
        for tenancy in ('dedicated', 'host', 'unknown', None, True):
            cloud = aws(); cloud.vpc['InstanceTenancy'] = tenancy
            with self.subTest(tenancy=tenancy), self.assertRaisesRegex(host.PilotError, 'default tenancy'):
                host.preflight(cloud, fixtures.specification(False), PAID)
            self.assertFalse(any(operation == 'describe-subnets' for _, operation, _ in cloud.calls))
        cloud = aws(); del cloud.vpc['InstanceTenancy']
        with self.assertRaisesRegex(host.PilotError, 'default tenancy'):
            host.preflight(cloud, fixtures.specification(False), PAID)

    def test_default_tenancy_evidence_binds_the_selected_vpc(self):
        cloud = aws()
        result = host.preflight(cloud, fixtures.specification(False), PAID)
        self.assertEqual((result['vpc_id'], result['vpc_tenancy']), ('vpc-test', 'default'))
        subnet_calls = [payload for _, operation, payload in cloud.calls if operation == 'describe-subnets']
        self.assertEqual(subnet_calls[0]['Filters'][0], {'Name': 'vpc-id', 'Values': [result['vpc_id']]})

    def test_existing_reservations_still_block_overspend(self):
        value = prepared()
        with tempfile.TemporaryDirectory() as directory, patch.object(host, 'STATE', Path(directory)):
            host.save(Path(directory)/'ledger.json', {'lifetime_cap_usd': '50', 'reservations': {'prior': {'reserved_usd': '49.99'}}})
            with self.assertRaisesRegex(host.PilotError, 'lifetime allowance'):
                host.reserve_budget(value)

    def test_exact_owned_cleanup_keeps_guard_until_terminal(self):
        value = dict(base.manifest(), host_profile=PAID)
        cloud = base.FakeAws(by_id=[base.instance()])
        with tempfile.TemporaryDirectory() as directory, patch.object(host, 'cleanup_control_plane') as cleanup:
            with self.assertRaisesRegex(host.PilotError, 'guard retained'):
                host.cleanup(cloud, value, Path(directory)/'run.json')
        self.assertEqual([payload for _, op, payload in cloud.calls if op == 'terminate-instances'], [{'InstanceIds': ['i-owned']}])
        cleanup.assert_not_called()


if __name__ == '__main__':
    unittest.main()

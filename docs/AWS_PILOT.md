# AWS guest pilot

Current status (2026-11-07): native x64 and ARM64 one-vCPU, 512 MiB Linux harnesses passed boot, acknowledged pause/quiescence, resume and hypervisor-initiated guest stop. The ARM CPU smoke also passed. The native direct-Linux x64 VMM integration also passed its separate lifecycle acceptance. CLI/RPC, firmware/security features and Windows remain unverified. All seven hosts and temporary resources are gone and independently audited. After explicit user approval, the account is Paid/Active with a saved $20 monthly pretax limit and all three early cost controls enabled; Basic Support is retained. Stockholm standard-instance quota 64 is applied. No next run is authorized by this document.

The user's authorization is **US$50 for the entire project**, including outstanding charges and taxes. Seven retained reservations total $7.97, a separate overlapping unsettled allowance remains $2.10, and provisional compute/storage/IPv4 components total $0.33, leaving **$39.60 conservatively available**. The ARM run's full $5.45 and latest VMM run's $0.42 reservations remain retained; their $0.21 and $0.01 component estimates are additionally deducted conservatively. This deliberate overlap is not a finalized bill. No credits are subtracted. The monthly $20 project limit is separate from the lifetime $50 ceiling; never reset the lifetime allowance for another run, architecture, region or month.

## Initial milestones (historical)

The first `m7i-flex.large` run used a maximum 171.1 seconds from EC2 launch to complete cleanup. Micro-VM execution and a real Linux kernel/minimal initramfs boot both passed. The latter reported KVM enabled, its PID1 serial marker, guest poweroff, and QEMU exit zero. No disk, network device, or host mount was attached to the guest. The conservative $0.42 reservation remains in the lifetime ledger pending billing reconciliation; the compute/storage/IPv4 portion alone is estimated at no more than $0.01 for that window. This is not a finalized bill. The account remained Free/Active.

The second run used a maximum 231.7 seconds through complete cleanup. It built the reviewed, hash-verified native source with official Rust/Cargo 1.99, passed all 16 Linux library tests, and exited zero with `native x86_64 smoke OK`. The example exercised the native backend's long-mode guest, PIO/MMIO read-back, cross-thread kick and resumed progress. Its source bundle was 2,264,761 bytes, SHA-256 `6dcc87c272cf7dec3a865ed381efab12b5d69f5a73a1b9d729cb95f3f2a979a3`; 125 SSM requests transferred and verified it without S3 or public hosting. At that historical milestone, reservations totaled $0.84 and an additional $0.42 unsettled allowance was retained. Native OS boot had not yet been tested; the fifth run subsequently passed the Linux harness acceptance described below.

## Initial reference profile

Start with one `m7i-flex.large` Linux host if the account's Free Plan permits it and regional preflight confirms nested virtualization. It has 2 vCPUs and 8 GiB RAM. `c7i-flex.large` has 4 GiB RAM and is slightly cheaper. Both are listed among eligible Free Plan types, and their families support nested virtualization. Do not upgrade the account automatically. Check eligibility on the actual account; budget conservatively at full published rates.

The reference run checked AWS access, `/dev/kvm`, ordinary Linux guest boot, and remote administration using QEMU. The subsequent native smoke validated the CPU/KVM backend, and the fifth native Linux harness verified the direct OS boot/control path. The seventh run subsequently verified the restricted direct-Linux x64 VMM profile; broader machine-model, CLI/RPC and firmware work remains separate. The reviewed ARM profile requires `c6g.metal`, an active Paid Plan, sufficient applied quota and capacity. Use a 30-minute cutoff including setup and boot; prepare artifacts before starting the billed host.

- [Free Plan EC2 eligibility](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-free-tier-usage.html)
- [Nested virtualization support](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/amazon-ec2-nested-virtualization.html)
- [Regional instance quotas](https://docs.aws.amazon.com/ec2/latest/instancetypes/ec2-instance-quotas.html)

Read the actual `ec2` quota `L-1216C47A`. Metal consumes the entire host's vCPU count: `c6g.metal` requires 64; `c7i.metal-24xl` requires 96. A quota increase does not establish available capacity or authorize a larger budget.

## Guest requirements

| Guest | Host path | Current vroom readiness |
| --- | --- | --- |
| Linux x64 | Intel nested KVM; metal later | Native harness and narrow direct-Linux VMM boot/control passed; CLI/RPC/firmware pending |
| Linux ARM64 | Graviton metal | Native CPU smoke and one-CPU Linux harness boot/control passed; full VMM/RPC/firmware pending |
| Windows 11 x64 | Dedicated infrastructure after license review | Full x86 machine board/firmware and platform security remain engineering work |
| Windows 11 ARM64 | Graviton metal after license/media review | Existing ARM path; protected Secure Boot variables not implemented |

AWS's Windows client BYOL guidance requires qualifying VDA E3/E5 subscriptions and dedicated infrastructure. Ordinary retail media or a shared Linux host cannot be assumed to grant Windows hosting rights. Dedicated metal is the narrow pilot policy, not a claim that it resolves every licensing condition. See the [AWS Windows FAQ](https://aws.amazon.com/windows/faq/).

`docs/SECURITY.md` documents that the current ARM variable store lacks write protection against the guest kernel. An enabled Secure Boot flag does not prove that protection. Require TPM 2.0 ready, OS volume fully encrypted with protection on and a TPM protector, protected firmware variables, and successful cold boot/unseal before calling the requested Windows combination complete. Never print recovery passwords or TPM secrets. `scripts/guest-readiness.ps1` and `scripts/guest-readiness.sh` provide read-only guest checks, not evidence of all platform guarantees. The Linux ARM64 KVM path in `crates/hv/examples/smoke.rs` passed its bounded runtime acceptance on the sixth host; GIC save/restore remains an API check rather than migration proof.

Nested to metal migration uses the same KVM API but is not guaranteed without validation. Recheck CPU capabilities, timers, firmware, vTPM state, and boot. Move disk, UEFI variables, and vTPM state consistently. Do not promise live snapshot portability across CPU models or hosts.

## Offline cost proposal

`scripts/aws-pilot.py` uses Python's standard library and has no AWS API or launch operation:

```sh
python3 scripts/aws-pilot.py template > /tmp/vroom-pilot-input.json
# The orchestrator fills verified prices and the cumulative project ledger.
python3 scripts/aws-pilot.py plan /tmp/vroom-pilot-input.json
python3 -m unittest discover -s scripts/tests -p 'test_aws_pilot.py' -v
```

The template is intentionally incomplete. Unknown spend/rates block a proposal. Input requires actual region/type, official quote checked within 24 hours, cumulative paid/accrued ledger checked within one hour, outstanding obligations, and evidence. Include all prior runs, control-plane resources, unbilled runtime, retained storage, traffic, and tax obligations. Delayed AWS billing alone is insufficient. Reserve the new run's estimate against the remaining **lifetime** allowance before any future launcher acts.

The estimate includes Linux On-Demand compute, baseline gp3 capacity, public IPv4, outbound traffic allowance, and a quoted control-plane allowance. It adds at least one hour of failure/cleanup reserve and at least 25% contingency; the template uses 50%. No credits or free allowances are subtracted. EBS prorating uses a conservative 28-day month. Extra gp3 IOPS/throughput, additional volumes, NAT gateways, Elastic IP allocations, load balancers, snapshots, and AMI copies are outside scope. The root disk must be encrypted and deleted on instance termination.

These are planning checks, not enforced AWS billing limits. The tool validates input consistency but does not fetch quotations, verify the ledger, reserve money, inspect an account, or terminate anything. Output always says `launch_enabled: false`. Freshness must be rechecked before any future launch. Contingency can cover tax and small variation but cannot cap a final bill.

Representative Stockholm Linux On-Demand rates retrieved 2026-11-07: `m7i-flex.large` $0.10175/hour; `c7i-flex.large` $0.09077/hour; `c6g.metal` $2.336/hour. Baseline gp3 is $0.0836/GB-month; public IPv4 is $0.005/hour. These are references, not an approved quote for the eventual account region. Sources: [official EC2 feed](https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/ec2/USD/current/ec2-ondemand-without-sec-sel/EU%20(Stockholm)/Linux/index.json), [official EBS feed](https://b0.p.awsstatic.com/pricing/2.0/meteredUnitMaps/ec2/USD/current/ebs.json), [IPv4 pricing](https://aws.amazon.com/vpc/pricing/).

New AWS signup projects may offer a monthly pretax spend limit with a minimum of $20 or the displayed estimate, whichever is higher. Check eligibility and behavior in [AWS spend-limit documentation](https://docs.aws.amazon.com/accounts/latest/reference/create-spend-limit.html). This differs from ordinary AWS Budgets notifications and the user's $50 lifetime all-in ceiling. Do not assume every account has this feature or choose a Paid Plan merely to obtain it.

## Launcher safeguards

`scripts/aws-host-pilot.py` implements the reviewed x64 nested and ARM metal profiles, using `scripts/aws-pilot-reaper.py` for independent expiry. Live x64 runs validated provisioning, SSM control, effective termination permission and complete cleanup; the third run also proved scheduled expiry through exact-instance/role CloudTrail attribution. The initial preflight confirmed Free/Active status, $100 initial credits, nested/Free eligibility, sufficient x64 quota, an existing public subnet and an Amazon-owned AL2023 AMI with exactly one root EBS mapping. These historical checks do not replace fresh checks for a later run. No account plan is upgraded by this tool.

The tool persists private manifests and an append-only conservative reservation ledger under the ignored `.aws-pilot/` directory, serialized by a local lock. Historical reservations are never automatically refunded; reported obligations are added conservatively, even if this double-counts some overlap. Cleanup completion does not erase a cost reservation. A live `preflight` creates only a local manifest. `launch`, `smoke`, and `cleanup` require the explicit `--execute-reviewed-plan` switch. Use the default `vroom-pilot` short-lived profile; root credentials are rejected.

A completely cleaned setup failure that never requested EC2 may be retried once with the original reservation and an audited new setup deadline. Once an EC2 request is issued, the cutoff cannot be extended. Only a definite IAM-profile propagation rejection is automatically retried, at most twice after the first request, using the identical payload and client token. Unknown failures retain the independent guard for reconciliation.

```sh
python3 scripts/aws-host-pilot.py preflight .aws-pilot/candidate-input.json --expected-account ACCOUNT_ID
python3 scripts/aws-host-pilot.py launch RUN_ID --execute-reviewed-plan
python3 scripts/aws-host-pilot.py status RUN_ID
python3 scripts/aws-host-pilot.py smoke RUN_ID --execute-reviewed-plan
python3 scripts/aws-host-pilot.py cleanup RUN_ID --execute-reviewed-plan
```

The operator must read the submitted SSM command's result; command submission is not test success. Cleanup may intentionally return pending while an instance terminates or an owned volume disappears; retry it and audit all tracked resources. A missing known instance is not accepted as proof of termination. An ambiguous launch without known IDs retains the independent guard for explicit account reconciliation. Never delete the guard merely because a tag query is temporarily empty.

The pre-armed guard's code hash, fixed deadline, IAM policy, EventBridge schedule/target, and invocation permissions are verified before and after EC2 launch. Before permitting workload submission, it must prove `TerminateInstances` permission on the exact live owned instance using AWS `DryRunOperation`. Any failed proof triggers parent-role cleanup. This is a layered safeguard, not an absolute billing guarantee; account policies or AWS service failures can still affect execution.

The launcher supplies plaintext EC2 `UserData`: the [AWS CLI encodes `run-instances` user data](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/user-data.html) itself, including JSON input. A local loopback regression exercises the installed CLI and confirms exactly one wire encoding. The previous input was pre-encoded; [cloud-init's EC2 source](https://github.com/canonical/cloud-init/blob/main/cloudinit/sources/DataSourceEc2.py) can decode that extra layer, and the live host's original timer was observed active. This correction does not imply that earlier bootstrap execution failed.

Before any workload, prepend the launcher's `bootstrap_check(manifest)` to its SSM shell command and inspect its successful result. This read-only check requires the matching run marker, KVM device, active transient timer, its exact original host deadline and shutdown action, and an unexpired earlier workload cutoff. Failure exits the calling shell before subsequent commands; it never creates or extends a timer. The built-in `smoke` command includes this check. Source-transfer and native-build operators must include it too. The timer's presence proves it is armed, not that future shutdown is guaranteed or that the separate scheduled reaper has been exercised.

1. Read account identity without exposing credentials. Verify plan eligibility, region, quota, architecture/features, Linux AMI provenance, permissions, capacity options, and quotation. Use short-lived non-root login. Reconcile previous resources and accrued costs.
2. Persist a unique run ID and local manifest before mutations. Record resource IDs, region, cutoff, price evidence, and costs; never credentials, vTPM material, or recovery keys. Use the run ID as the EC2 idempotency token. Permit exactly one instance.
3. Arm independent AWS-side expiry **before launch**. A small scheduled Lambda can find only this run's exact project/run tags in the fixed account/region, then terminate matching instances after its fixed UTC cutoff. Its termination policy must require both tags. It must not trust an editable instance tag to extend its deadline. Invoke and inspect the guard before launch. A host shutdown timer alone is insufficient.
4. Invoke the guard every minute and retry termination until owned instances are gone. Include invocation/log costs in the quote. A direct Scheduler `terminateInstances` target with an exact ID is simpler after launch, but leaves an unprotected gap before the ID is known; it cannot alone meet the pre-armed requirement. The tag-scoped guard addresses this gap. Live IAM and failure-path validation is still required.
5. Launch with an encrypted bounded gp3 root, delete-on-termination, shutdown behavior `terminate`, IMDSv2, and no unnecessary instance permissions. Add a host timer as a second guard. Restrict administration to the operator; do not expose guest consoles/RPC publicly. Avoid paid network appliances. Include `CpuOptions.NestedVirtualization=enabled` only for a verified supported host. The cutoff includes host boot/setup.
6. Recheck independent expiry immediately after launch. On setup or verification failure, stop the experiment and terminate only the exact manifest-owned instance. Never perform broad account cleanup. Run read-only host/guest checks and terminate early when finished.
7. Confirm termination and deletion of the owned root volume and public IP association. Remove the guard/schedule/IAM resources only once no owned instance survives. Audit for retained volumes, snapshots, addresses, logs, and other owned resources. Keep costs reserved until delayed charges reconcile; an empty instance list does not settle the lifetime ledger.

The implementation uses a scheduled EventBridge rule and Lambda. EC2 Fleet was considered for atomic expiry but the new-project policies explicitly deny `ec2:CreateFleet` on Free and Paid project plans; do not upgrade to Advanced to evade this restriction. See [project policy documentation](https://docs.aws.amazon.com/accounts/latest/reference/scps-and-rcps-for-projects.html). If account restrictions prevent the independent guard, do not silently weaken protection or upgrade the account.

### Optional scheduled-expiry self-test

For a separately reviewed run, add `--expiry-self-test-after-minutes N` to `preflight`. `N` must be at least five minutes and at least five minutes shorter than the original quoted runtime. There is no default test cutoff. Select it only after allowing for measured setup, upload, build and workload bounds; do not extend it after requesting EC2. The mode preserves the specification, full-runtime cost reservation and host shutdown allowance. It changes only the scheduled guard/workload cutoff to an earlier time. Both deadlines are fixed before guard creation; exact EC2 retries reuse the same request.

The manifest's existing `deadline` remains the earlier guard and workload cutoff. `host_shutdown_deadline` is the unchanged original host fallback, calculated from the same launch start (rounded down to seconds to match the systemd timer). Every launch/workload precheck uses `deadline`; the later fallback is not extra usable workload time. Normal runs and historical manifests keep their existing single-deadline behavior. Ambiguous launches retain their guard through the later host cutoff plus two minutes, and missing IDs still require explicit reconciliation.

After the workload completes, observe the scheduled guard rather than manually invoking expiry. `python3 scripts/aws-host-pilot.py expiry-status RUN_ID` verifies the guard, reads the exact manifest-owned instance ID and records its first observed terminal state. It performs no AWS mutations. Poll around the earlier cutoff with a bounded observation window; keep the host timer and normal cleanup available on failure. An instance already observed terminal before the guard cutoff never becomes a passing expiry observation later. A missing instance or termination first observed at/after the host fallback is not timely evidence.

`expiry-status` deliberately reports attribution as `unverified` and `self_test_passed: false`: timing alone cannot identify who terminated the host. Before claiming scheduled expiry success, inspect a successful CloudTrail `TerminateInstances` management event for the exact instance, fixed account/region and exact reaper role session issuer. Require event time at/after the guard cutoff and strictly before the host timer, with no error; reject operator-role, host-initiated, wrong-ID and dry-run events. Preserve only the event ID/time, instance ID and caller-role identity, not full credential-bearing event records. Missing or delayed attribution remains unverified. Do not retain compute or extend either deadline while waiting for CloudTrail. Once exact-instance termination is confirmed, use normal cleanup and audit volumes, public IP association, rule, function and roles; a failed self-test never disables cleanup or releases the budget reservation.

## Preparing the native Linux harness

The reviewed `vroom-linux-harness` is an intermediate acceptance binary using the native hypervisor, direct Linux loader and COM1 device. It is separate from the VMM CLI/RPC. See `crates/linux-harness/README.md` for acceptance and limitations. Native x64 Linux boot, acknowledged pause/quiescence, resume and hypervisor-initiated guest stop passed on nested KVM in the fifth pilot described below.

```sh
python3 scripts/prepare-hv-smoke.py stage .aws-pilot/linux-harness-stage --example linux_harness
python3 scripts/prepare-hv-smoke.py pack .aws-pilot/linux-harness-stage .aws-pilot/linux-harness-reviewed.tar.gz --reviewed-source
```

This profile adds the exact eight required first-party crates and the deterministic heartbeat helper/tests. It excludes research directories, the disabled block research module, guest images and credentials. The measured bundle contains 82 first-party files and 55 pinned registry packages: 10,330,496 compressed bytes and 58,114,280 file bytes unpacked. The transfer tool now allows at most 12 MiB compressed and still caps extraction at 64 MiB. Its 561 data chunks plus bootstrap/extraction require 563 SSM requests, each at most 25,230 bytes, with at most four in flight.

The previous 125-request transfer took about 40 seconds between first and last submissions; proportional scaling gives about three minutes, with three to five minutes allowed for this larger transfer. The generated wrapper bounds native library tests/build to 300 seconds plus five seconds kill grace, the heartbeat helper's actual Linux-process test to eight seconds plus two seconds grace, and the guest harness to 70 seconds plus two seconds grace. Use an SSM execution timeout of at least 420 seconds and require at least eight minutes before the guard deadline when starting the build. If time is insufficient, terminate and report the incomplete milestone; do not extend a live deadline.

The third run retained the same Free Plan, single `m7i-flex.large`, 40 GiB encrypted disposable root, no ingress and full $0.42 reservation. Its scheduled reaper cutoff was 15 minutes after setup started, with the original 30-minute host fallback unchanged. Transfer was stopped after 220 verified chunks when its measured rate could not meet the eight-minute build gate; no native build or guest workload ran. The host stayed idle for the separately authorized expiry test.

That scheduled-expiry test passed on 2026-11-07: the cutoff was 13:15:13 UTC, CloudTrail attributed the exact instance's successful `TerminateInstances` call at 13:15:14 UTC to the exact reaper role, and termination was positively observed at 13:15:49 UTC, before the 13:30:13 host fallback. All disposable storage/network/guard/IAM resources were removed and independently audited absent. An eventual terminated state alone was not used as attribution. The run's compute/storage/IPv4 upper estimate was $0.04, excluding control-plane, traffic and taxes; its full $0.42 reservation remains retained.

During this run, review found redundant base64 encoding in EC2 user data. The actual host timer was already active because cloud-init decoded the additional layer. The launcher now passes canonical plaintext to the CLI's encoding handler, verified through a local CLI serialization test. Future install, transfer and workload commands additionally verify the actual timer's exact original deadline/shutdown action, run marker and KVM device before proceeding.

The fourth attempt reused the identical reviewed artifact with the normal 30-minute guard/fallback and the same $0.42 gross reservation. All 109 native Rust tests and five heartbeat-helper tests passed on the host; the native binary built successfully. The wrapper then rejected the official versioned `kernel6.18` package because its package-name check only allowed `kernel` or `kernel-core`. No guest was started. Read-only follow-up confirmed Amazon Linux package metadata/signature and recorded kernel/binary hashes, but the generated `/boot/vmlinuz` file had no RPM file digest. No live patch or guest rerun followed, and the host was cleaned up.

Subsequent offline inspection of the exact official RPM resolved the layout: the canonical `/lib/modules/<release>/vmlinuz` payload has a recorded SHA256 that exactly matches the fourth host's boot copy. The `/boot` entry is an expected RPM ghost file. The independently reviewed shared resolver now selects and verifies that actual packaged payload before the bounded build and guest execution.

The fourth host, its exact known root volume and all temporary networking/guard/IAM resources were removed and independently audited absent. Its conservative compute/storage/IPv4 component estimate was $0.03. Four retained run reservations total $1.68; their combined provisional component estimates total $0.09, excluding control-plane, traffic and taxes and without claiming a finalized bill. An additional overlapping $1.68 unsettled allowance leaves $46.55 conservatively available under the $50 lifetime authorization. Credits are not subtracted from estimates, and reservations are not refunded automatically.

The one separately authorized request to raise the Stockholm standard-instance quota from 32 to 64 with support-case creation disabled was `NOT_APPROVED`. No support case was opened, no duplicate request was sent, and the account plan was not upgraded.

The fifth pilot on 2026-11-07 passed native Linux harness acceptance using the newly reviewed resolver/wrapper archive `f0c187d1c0af54152588187608a900c483171fda45a68d8e3647eebaa05d2809` (10,335,201 bytes). All 3,278 inventoried files were verified after 563 SSM requests; transfer took approximately 182 seconds from first to last submission. The original x64 Rust and heartbeat sources were unchanged. Before compilation, the canonical Amazon Linux `kernel6.18-6.18.56-125.164.amzn2023.x86_64` payload matched its recorded RPM SHA256 `537cd20f54e670b9e855d0b72b8b340a2c8f526af6449d94675bfb7c363a1068`.

All 109 native Rust library tests and five heartbeat-helper tests passed. The actual one-vCPU, 512 MiB Linux guest produced five exact sequential PID1 heartbeats, acknowledged pause, made no heartbeat or serial-byte progress for 350 ms, resumed to ten heartbeats, and stopped through the hypervisor with a successful worker join. The complete build/test/workload SSM command finished in approximately 69 seconds with exit zero and complete captured output. This verifies the isolated native harness; full VMM/RPC integration, guest ACPI poweroff, serial receive/IRQ delivery, UEFI/Secure Boot, TPM, BitLocker and Windows remain unverified.

The fifth host was terminated early and all temporary storage, networking, guard, logs and IAM resources were removed; the exact instance, known root volume and resource absence were independently audited at 13:54:51 UTC. The Free Plan remained active. Its conservative compute/storage/IPv4 component estimate was $0.02, excluding control-plane, traffic and taxes. Five retained $0.42 reservations total $2.10; combined provisional component estimates total $0.11. Retaining a separate overlapping $2.10 unsettled allowance leaves $45.69 conservatively available under the $50 lifetime authorization. These are conservative accounting figures, not a finalized bill; credits are not subtracted and reservations are not automatically released.

## Preparing the native Rust smoke locally

`scripts/prepare-hv-smoke.py` prepares a standalone workspace containing only the Linux-relevant `vroom-hv`/`vroom-memory` source and one selected smoke example. It prunes the seeded repository lockfile, rejects changed dependency versions/checksums, and vendors the remaining pinned crates. Research, other workspace crates, VM images, credentials, `.aws-pilot` state, build outputs, and unrelated examples are not copied. The draft dependency closure is 19 registry crates, about 12.4 MB unpacked and 2.2 MB compressed; final sizes and hashes must be measured after source review.

Wait for the native source author and reviewer before freezing an artifact:

```sh
python3 scripts/prepare-hv-smoke.py stage .aws-pilot/native-reviewed-stage --example smoke_x86_64
python3 scripts/prepare-hv-smoke.py pack .aws-pilot/native-reviewed-stage .aws-pilot/native-smoke.tar.gz --reviewed-source
```

Staging uses the existing local crate cache by default. `--allow-fetch` permits Cargo to fetch the pinned crates locally if needed; it does not enable host-side build downloads. Packing verifies that source/context hashes have not changed since staging, fixes archive metadata for reproducibility, includes a per-file hash inventory, rejects symlinks, and caps uncompressed content at 64 MiB. A local draft cross-check with an empty Cargo home successfully used only vendored dependencies. This validates packaging, not KVM execution on macOS.

After a separately approved new bounded host run, generate SSM request JSON files locally using its exact run/instance IDs:

```sh
python3 scripts/hv-smoke-transfer.py generate .aws-pilot/native-smoke.tar.gz .aws-pilot/native-transfer --run-id RUN_ID --instance-id INSTANCE_ID
```

This command sends nothing. It creates an ordered bootstrap, indexed chunk requests, and final verification/extraction request. Each payload is at most 32 KiB, conservatively below the documented [SSM 64 KiB document limit](https://docs.aws.amazon.com/systems-manager/latest/APIReference/API_SendCommand.html). Binary chunks are at most 18 KiB before base64 encoding; the draft bundle needs roughly 120 requests. The final generated plan reports exact sizes and hashes. Use the existing SSM channel; no S3 bucket, public URL, SSH ingress, or new credentials are needed.

The cloud operator must reverify instance ownership, the exact profile's active account plan, budget reservation, original deadline, and the independent guard. Complete and inspect the bootstrap first. Then dispatch chunks in index order, with at most four in flight to distinct part files; record command IDs and await successful invocations for every part. Identical chunk retries are idempotent. Stop on a failed hash, wrong ownership, deadline pressure, or persistent service error. Only after all parts succeed may the final request assemble the archive, verify its complete hash, reject traversal/links/duplicate paths/oversized members, and verify every extracted file. Inspect `verified: true` and the exact expected archive hash before building.

Use official AL2023 packages for the build toolchain. [The 2023.12.20261026 release notes](https://docs.aws.amazon.com/linux/al2023/release-notes/relnotes-2023.12.20261026.html) include Rust 1.99. Preview the exact host transaction before any install:

```sh
dnf --assumeno --setopt=install_weak_deps=False install rust cargo gcc
```

If the transaction fits the existing transfer/storage/time allowance, the operator can install that reviewed package set. Do not install rustup or fetch a shell installer. The bundled `build-and-run.sh` records compiler versions, uses a private Cargo home, performs the hypervisor library tests and `cargo build --frozen` with two jobs under a combined 180-second limit, then runs only the selected smoke example under a 15-second outer limit. The build uses vendored dependencies with Cargo networking disabled. An unsupported platform or build failure is not a passing native test. Save stdout/stderr, exit status, artifact hash, and toolchain versions, then clean up all temporary AWS resources using the original deadline and ownership rules. Do not extend a paid host's lifetime to debug packaging.

## ARM64 artifact preparation (historical)

The following records the preparation state before the sixth run. It is superseded by the current status above and actual ARM acceptance below; it does not authorize a new reservation or launch.

The local ARM64 artifact is `.aws-pilot/arm64-native-smoke-reviewed.tar.gz`, SHA256 `25078d1b27ee73ca58606e82a3df76aa746a59b48de70836e1bcc339d6090b5b`, 2,266,478 compressed bytes. Its 1,180 inventoried files were hash-verified, and its staged sources passed `cargo check --frozen -p vroom-hv --all-targets --target aarch64-unknown-linux-gnu`. The bundle profile and 15 packaging tests received independent review. The existing x64 artifact and wrapper contents remain unchanged.

At this early preparation stage, Linux ARM KVM execution had not been tested. The guest is eight ARM64 instructions with one vCPU and 1 MiB RAM. Acceptance requires successful native library tests, exact MMIO results including read completion and changed CPU-state restoration, PSCI shutdown, exit zero and the final `smoke OK` line. GIC save/restore checks its API only. It does not prove Linux OS boot, Windows support, UEFI, TPM, BitLocker or snapshot portability.

The local `.aws-pilot/arm64-native-smoke-plan.json` records the bounded execution: 125 SSM transfer requests, at most four in flight, 180 seconds for tests/build plus five seconds kill grace, and a 15-second outer smoke limit around its 10-second watchdog. Start the build only with at least five minutes remaining in the original host window. Verify the actual host timer and independent guard first; never extend the deadline to finish setup.

No ARM cloud launch is enabled by this artifact. The launcher now has a separately reviewed `arm64-metal-paid` profile fixed to `c6g.metal` in Stockholm with a 40 GiB encrypted disposable root. It requires an active Paid Plan, explicit account authorization, fresh pricing and budget reservation, sufficient quota, regional availability, and an Amazon-owned ARM AL2023 AMI. At this preparation stage, the account was still on the Free Plan. The tooling performs no plan upgrade.

The combined ARM CPU-smoke and Linux-harness source artifact is `.aws-pilot/arm64-linux-final-reviewed.tar.gz`, SHA256 `d55b67ca94b506ebc31183a7ed3f26eb5a7dc7c076ef6a3c7dbe4562a1c1a7e2`, 10,347,720 bytes. Independent review verified all 3,284 inventoried files, 90 first-party files, 55 pinned registry packages and generated/context hashes; a deterministic repack matched byte-for-byte. Its 58,650,024 total extracted bytes fit the 64 MiB cap. The frozen vendored source passed the ARM Linux all-target check with an empty Cargo home. This was compile-time validation; actual ARM runtime was tested separately in the sixth run below.

Prepare the combined source profile locally with `--example linux_harness_arm64`. The fixed wrapper first verifies canonical RPM kernel/config hashes and required built-in ARM drivers, then runs native library tests, the CPU smoke, heartbeat-helper tests, and the one-vCPU, 512 MiB Linux harness. It has a 420-second whole-wrapper timeout plus five seconds kill grace; use an SSM timeout of 450 seconds and require at least eight minutes before the unchanged host cutoff. The local transfer preview contains 562 data chunks and 564 total requests, at most four in flight, without sending anything.

The disabled `.aws-pilot/arm64-linux-final-plan.json` proposes one 30-minute `c6g.metal` run at the verified Stockholm Linux rate of $2.336/hour. Including one extra hour of cleanup reserve, storage, public IPv4, bounded traffic/control-plane allowances and 50% contingency gives a conditional $5.45 reservation. No ARM reservation existed at this historical stage: the allowance was $45.69, conditionally $40.24 after the proposed reservation. The current reconciliation is stated above. No credits are subtracted. AWS documents that [bare-metal startup can take 20 minutes or more before networking](https://docs.aws.amazon.com/ec2/latest/instancetypes/ec2-nitro-instances.html); the fixed 30-minute deadline may therefore force cleanup without running the workload. It must not be extended automatically.

The adopted compact alternative is `.aws-pilot/arm64-prebuilt-final-v5-reviewed.tar.gz`, SHA256 `9f5940ce1b7bfbe9e44fe170721b576d9df225039822e46853a59bea17cf31ae`, 4,154,735 bytes. It contains the same reviewed source's statically linked `aarch64-unknown-linux-musl` harness, CPU smoke and four native library-test executables, plus unchanged kernel/heartbeat helpers. All six binaries are AArch64 ELF with no interpreter or dynamic-library dependency, 64 KiB load alignment and no writable/executable load segment. The package records compiler, target-standard-library, source, dependency and binary hashes; no new toolchain download was needed. These are build/provenance checks, not ARM runtime evidence.

Independent review verified all 15 inventory entries and 228 v5 transfer requests. The wrapper rechecks the six exact binary hashes and restores executable permissions after the shared receiver's conservative extraction. Actual receive/extract/mode recovery and changed-binary refusal regressions passed. Use the explicitly versioned v5 archive and preview; earlier prebuilt drafts are superseded. The disabled `.aws-pilot/arm64-prebuilt-final-plan.json` removes host-side compiler installation/build, allows at most four concurrent transfer requests, bounds the whole workload to 180 seconds plus five seconds kill grace, uses a 210-second SSM timeout and requires at least five minutes before the original host cutoff. The 30-minute host window and conditional $5.45 reserve remain unchanged. The source artifact is retained for reproducibility, with no automatic source-build fallback or guest rerun.

The separately authorized support-backed quota request created one case. Its closed status represented a successful resolution: the orchestrator directly observed AWS Support's approval of quota 64, effective within 30 minutes of 14:01:58 UTC on 2026-11-07. This browser evidence is distinct from the latest applied-quota API value of 32 at 14:05:12 UTC. Reverify the applied limit before any ARM preflight; do not duplicate or reopen the request. At that historical stage, no Paid Plan upgrade, ARM launch or ARM reservation had occurred. The sixth run below records their subsequent authorized completion.

## Sixth pilot: native ARM CPU and Linux acceptance

The adopted v5 static package ran successfully on one `c6g.metal`, with a 40 GiB encrypted disposable root, no ingress and an unchanged 30-minute cutoff. All 228 SSM requests succeeded and verified the exact archive and 15 files; first-to-last submission took about 71 seconds. Nothing was installed or compiled on the host. The actual workload exited zero with complete output: 114 native Rust tests, 19 helper tests, `smoke OK`, and Linux acceptance JSON. Submission to captured result took 7.12 seconds.

The packaged Amazon Linux `kernel6.18-6.18.56-125.164.amzn2023.aarch64` kernel and configuration matched their recorded RPM SHA256 values. Kernel hash: `5dfabd21e449b3263495b1a2c901c4f42a5b486c42b80f2af3ee5ce24d88093a`; configuration hash: `884ebfc0bbe63c582503a48d255cc6d65e3e0a481ae69d7ff1829df7082f529f`. The wrapper extracted an ARM Image from the compressed payload and booted it directly. It did not boot EFI firmware.

The one-vCPU, 512 MiB ARM guest had no disks or NICs. It produced five sequential PID1 heartbeats, acknowledged pause, stayed quiescent, resumed to ten heartbeats, and completed controller stop with worker cleanup. The separate CPU smoke checked MMIO/read-back, changed CPU-state restoration and PSCI system-off. Full VMM/CLI/RPC, Windows, UEFI/Secure Boot, TPM, BitLocker, Linux guest poweroff, serial RX/IRQ delivery and migration remain outside this acceptance.

CloudTrail records running to shutting-down at 16:02:18 UTC, 311 seconds after EC2 LaunchTime. Terminal state was first positively recorded at 16:15:09 UTC, known root-volume absence at 16:15:34, complete cleanup at 16:15:36, and independent absence audit at 16:16:13. Force termination was skipped because its fresh precheck already found the instance terminal. The original 16:26:21 cutoff was never extended. Applied quota 64 and Paid/Active state were reverified before launch; the earlier quota/support and Free-state notes above are historical.

The compute/storage/IPv4 estimate is $0.21, summed before rounding upward to cents, excluding control-plane calls, traffic and tax. Compute ends at the recorded shutting-down transition; storage and IPv4 use conservative disposal bounds. [AWS lifecycle documentation](https://docs.aws.amazon.com/AWSEC2/latest/UserGuide/ec2-instance-lifecycle.html) states that compute charges stop in shutting-down. The full $5.45 reservation stays in the lifetime ledger. Private evidence is in `.aws-pilot/vroom-pilot-3600a076ff50.report.json`, its independent audits, and `project-budget-reconciliation-after-arm.json`.


## Seventh pilot: native x64 VMM lifecycle acceptance

The separately reviewed `Vmm::start` integration passed on one `m7i-flex.large` under the explicit Paid profile. The default Free profile remains strict. Static archive `3269cd311fd5df8d0311cb0c58b4e477dfa20d4b97a85fa7f13e5ef86da75f2b` was 7,950,188 bytes; all 434 SSM requests and 15 inventory files verified. Transfer took 133.73 seconds between first and last submission. Nothing was installed or compiled on the host. The original 17:02:47 UTC cutoff was never extended.

All 175 native tests passed (21 architecture, 69 devices, 16 hypervisor, 66 VMM, and three example tests), followed by 17 helper tests. The canonical Amazon Linux x64 kernel matched RPM SHA256 `537cd20f54e670b9e855d0b72b8b340a2c8f526af6449d94675bfb7c363a1068`. The one-vCPU, 512 MiB direct-kernel guest had no disks or NICs. BootPaused returned with zero worker run attempts, the existing sandbox API applied before first resume, five sequential PID1 heartbeats preceded acknowledged pause/quiescence, resume reached ten heartbeats, and checked shutdown joined the workers. The supervising process removed its private state directory. Exactly one guest invocation ran.

The wrapper took 23.481 seconds and exited zero; complete SSM output was 2,548 bytes with empty stderr. Independent review verified the actual command result and all acceptance flags. `sandbox_apply_succeeded` proves the API call returned successfully; Landlock enforcement and complete isolation are not claimed. CLI/RPC, guest poweroff, snapshots, serial input/IRQ delivery, UEFI/Secure Boot, TPM, BitLocker and Windows remain outside this narrow acceptance. See `crates/vmm/X64_LINUX.md` for the supported profile.

The host was terminated immediately after recording results. Complete cleanup finished at 16:38:52 UTC, the exact instance was directly observed terminal and its root volume absent at 16:38:56, and independent absence audit passed at 16:39:20. The compute/storage/IPv4 component estimate is $0.01, excluding control-plane, traffic and tax; the full $0.42 reservation remains retained. Evidence is `.aws-pilot/vroom-pilot-282f5876653f.report.json` and its independent audits; current conservative lifetime reconciliation is `project-budget-reconciliation-after-vmm.json`.

## Local x64 QEMU firmware/TPM acceptance

The separately reviewed local QEMU reference campaign passed on 2026-11-07:
three cold starts (enrollment, Linux seal, Linux unseal), using the reviewed
spawn/join lifecycle and three guest-shutdown records. External process
observation separately captured the third boot's QEMU/swtpm PIDs. The accepted
campaign took 14.909 seconds; including the preserved initial failed attempt,
aggregate execution was 19.070 seconds. It used one emulated x64 CPU, 1 GiB RAM,
read-only FAT media, no guest network or host-directory sharing, and private
Unix sockets only. No AWS resources, account changes, or reservations were used.

Fresh guest-only PK/KEK/db enrollment transitioned SetupMode from 1 to 0. Both
Linux boots then required SecureBoot=1/SetupMode=0 and identical enrolled
certificate fingerprints. Well-formed authenticated updates signed by an
unenrolled certificate were rejected; correctly KEK-signed updates preserving
the same db bytes succeeded. Exact values and attributes were re-read. The
signed EFI positive control executed once per boot. The known unsigned image
was refused with documented EFI_ACCESS_DENIED and a null image handle; it was
never started. Both the initrd and unsigned test image were hash-bound by the
signed loader. The initrd was not itself an Authenticode-signed image.

Linux accessed the emulated TPM2 through its normal kernel device. Each boot
replayed eight SHA256 PCR7 events, matched the live PCR, and verified a measured
SecureBoot=1 value. A real TPM2_Create/Load/Unseal object required PCR policy:
password-only and empty-policy authorization were rejected, while PolicyPCR
unseal succeeded. Only wrapped object blobs and metadata were persisted in
fresh TPM NV. After restarting both QEMU and swtpm, the same primary/object was
reloaded and unsealed with matching PCR7, sealed-blob and value hashes. No
private signing key or unsealed value was logged.

The controller required zero QEMU exit codes, guest-shutdown events, all firmware and
TPM gates, and complete process cleanup. Independent audit confirmed the exact
accepted report, no remaining owned processes, and removal of the observed
private TPM/NVRAM directory. Disposable signing keys had already been deleted
before guest execution. Raw private serial/swtpm logs were deleted by design;
retained evidence consists of parsed statuses, public hashes and observations.
The initial candidate-v2 attempt remains FAIL: enrollment passed, but its overly
narrow expected unsigned-image status stopped execution before Linux. Corrected
candidate-v3 was independently reviewed and separately authorized; no automatic
retry occurred.

Reproducibility evidence is under `.aws-pilot/firmware-reference/`:

- `candidate-v3/manifest.json`, SHA256
  `4ff54622d1a94c87620a0bf02b589e23b4812d20344e6d1e48ca947ceff944b2`,
  binds installed tools/firmware, kernel, static PID1, sources and signed media.
- `GO_PROPOSAL_V3.json`, `prepare.py`, `run.py`, `src/` and
  `pid1/BUILD_MANIFEST.json` record exact preparation, launch and build inputs.
  The used campaign is single-use; reproduction requires a fresh reviewed
  candidate and explicit authorization, not a retry of the completed command.
- `campaign-v3-report.json` and `INDEPENDENT_AUDIT_V3.json` record acceptance and
  cleanup. `campaign-v2-report.json`, its audit and `history/v2/` preserve the
  earlier failure. Sixteen fixture regressions and fourteen PID1 parser/command
  tests also passed; the helper-only codec test remains separate evidence.

This proves only the reviewed Linux x64 QEMU/EDK2/swtpm reference configuration
and normal authenticated-variable API enforcement. PCR/event-log consistency
is not independent firmware attestation or proof of variable-storage attack
resistance. Native vroom firmware/Secure Boot/TPM integration, Windows,
BitLocker, ARM firmware, and migration remain unproved by this campaign.

The independently reviewed native [TPM capture fix](../crates/vmm/TPM_STATE.md)
is promoted: failed exports or required-file reads propagate errors instead
of producing partial successful snapshots. Unit/lifecycle tests and a separate
disposable-helper adapter check passed. Valid-input restore is still
nontransactional. Neither helper test establishes native firmware security,
Windows/BitLocker acceptance or migration portability. No new AWS run or
reservation is authorized by this preparation.

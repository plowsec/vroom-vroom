# HackSys Extreme Vulnerable Driver

HEVD is the first Windows kernel target for the vroom research runner. The
target is intentionally vulnerable and belongs in an isolated, disposable VM.
Keep its source, signed driver and guest state outside this repository.
The manifest example is pinned to upstream revision
`b1cc756e55f69530f6468a6c3c310ed7b967bada`.

The current upstream tree has a Windows ARM64 build. Build the vulnerable
configuration on a Windows host with the WDK:

```bat
Builder\Build_HEVD_Vulnerable_arm64.bat
```

Install the resulting driver in the guest, run a small client that opens
`\\.\HackSysExtremeVulnerableDriver`, and stop the VM immediately before the
`DeviceIoControl` call. The checkpoint must record the paused CPU, the physical
input page (`input_gpa`), the user virtual input pointer (`input_va`) and the
instruction after the call (`stop_pc`). Those three addresses are specific to
the guest and must be filled into [harness.example.json](harness.example.json).
The small [client.c](client.c) is provided to make that call-site setup
repeatable.

The testcase format follows HEVD's call convention:

```text
u32 little-endian IOCTL | arbitrary input bytes
```

The runner writes the payload to the saved input page, puts the IOCTL in `x1`,
the input pointer in `x2`, the payload length in `x3`, and stops at `stop_pc`.
The manifest's coverage blocks come from the driver disassembly. Use the
Ghidra script to produce the block list, then fill each block's physical GPA
from the checkpoint's page tables before running it.

```sh
python3 research/hevd/verify.py \
  --header /path/to/HackSysExtremeVulnerableDriver/Driver/HEVD/Windows/HackSysExtremeVulnerableDriver.h \
  --harness research/hevd/harness.example.json

cargo run -p vroom-fuzz -- check research/hevd/harness.example.json
cargo run -p vroom-fuzz -- run research/hevd/harness.example.json seed.bin --iterations 1000
```

The verifier checks that the checked-out HEVD header exposes the expected IOCTL
surface and that the manifest uses the ARM64 call registers and required
diagnostic classes. It does not execute a driver. A real campaign requires a
Windows ARM64 guest, a matching checkpoint and a driver disassembly with
physical addresses resolved.

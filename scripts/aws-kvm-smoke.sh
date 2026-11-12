#!/bin/sh
# Linux x86_64 host reference: one KVM vCPU, 4 KiB, MOV/OUT/HLT only.
# No guest network, disks, downloaded artifacts, or software emulation fallback.
# This does not test native vroom x64, an OS boot, Secure Boot, TPM or BitLocker.
# Needs only Python 3 and GNU coreutils timeout (Ubuntu/AL2023 base packages).
# Exit 0: verified KVM execution; 1: failed; 2: unknown/timeout/prerequisite missing.
# ABI: https://docs.kernel.org/virt/kvm/api.html
set -u
if [ "$#" -ne 0 ]; then
    printf '%s\n' '{"schema_version":1,"status":"unknown","scope":"host_kvm_reference_only","native_vroom_x64_tested":false,"error":"this bounded smoke accepts no arguments"}'
    exit 2
fi
if ! command -v python3 >/dev/null 2>&1 || ! command -v timeout >/dev/null 2>&1; then
    printf '%s\n' '{"schema_version":1,"status":"unknown","scope":"host_kvm_reference_only","native_vroom_x64_tested":false,"error":"requires python3 and GNU coreutils timeout"}'
    exit 2
fi

smoke_output=$(timeout --signal=TERM --kill-after=2s 10s python3 - <<'PY'
import ctypes
import errno
import fcntl
import json
import mmap
import os
import platform
import struct
import sys
from contextlib import ExitStack

# Linux x86_64 UAPI ioctl encodings and structure sizes. No device emulation.
KVM_GET_API_VERSION = 0xAE00
KVM_CREATE_VM = 0xAE01
KVM_GET_VCPU_MMAP_SIZE = 0xAE04
KVM_SET_TSS_ADDR = 0xAE47
KVM_CREATE_VCPU = 0xAE41
KVM_SET_USER_MEMORY_REGION = 0x4020AE46
KVM_GET_SREGS = 0x8138AE83
KVM_SET_SREGS = 0x4138AE84
KVM_SET_REGS = 0x4090AE82
KVM_RUN = 0xAE80
KVM_EXIT_IO = 2
KVM_EXIT_HLT = 5
RAM_BYTES = 4096
IO_PORT = 0xE9
EXPECTED_BYTE = 42
# 16-bit real mode: mov dx, 0xe9; mov al, 42; out dx, al; hlt.
CODE = bytes((0xBA, 0xE9, 0x00, 0xB0, EXPECTED_BYTE, 0xEE, 0xF4))


class VerificationFailed(Exception):
    pass


def require(condition, message):
    if not condition:
        raise VerificationFailed(message)


def verify_exit(run, expected):
    """Accept exactly the harmless guest's OUT byte followed by HLT."""
    reason = struct.unpack_from("<I", run, 8)[0]
    require(reason == expected, "unexpected KVM exit reason {} (expected {})".format(reason, expected))
    if reason == KVM_EXIT_IO:
        direction, size, port, count, offset = struct.unpack_from("<BBHIQ", run, 32)
        require((direction, size, port, count) == (1, 1, IO_PORT, 1), "unexpected guest I/O operation")
        require(0 <= offset < len(run), "guest I/O data offset outside KVM mapping")
        require(run[offset] == EXPECTED_BYTE, "guest I/O byte did not match the instruction result")
    return reason


def run_smoke(system, machine):
    result = {
        "schema_version": 1,
        "scope": "host_kvm_reference_only",
        "engine": "linux_kvm_ioctl",
        "native_vroom_x64_tested": False,
        "os_boot_tested": False,
        "security_features_tested": False,
        "host_os": system,
        "host_architecture": machine,
        "limits": {"vcpus": 1, "guest_ram_bytes": RAM_BYTES, "timeout_seconds": 10, "kill_grace_seconds": 2},
        "guest_network_devices": 0,
        "guest_disk_devices": 0,
        "kvm_api_version": None,
        "vm_created": False,
        "vcpu_created": False,
        "verified_exits": [],
        "status": "fail",
    }
    stage = "host_preflight"
    try:
        require(system == "Linux" and machine.lower() == "x86_64", "requires a Linux x86_64 host")
        require(ctypes.sizeof(ctypes.c_void_p) == 8 and mmap.PAGESIZE == RAM_BYTES,
                "requires 64-bit Python and 4 KiB host pages")
        # Close all KVM descriptors before releasing their backing memory.
        with mmap.mmap(-1, RAM_BYTES, flags=mmap.MAP_SHARED | mmap.MAP_ANONYMOUS) as ram, ExitStack() as resources:
            stage = "open_kvm"
            kvm_fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
            resources.callback(os.close, kvm_fd)
            stage = "api_version"
            result["kvm_api_version"] = fcntl.ioctl(kvm_fd, KVM_GET_API_VERSION, 0)
            require(result["kvm_api_version"] == 12, "KVM API version is not 12")
            stage = "create_vm"
            vm_fd = fcntl.ioctl(kvm_fd, KVM_CREATE_VM, 0)
            resources.callback(os.close, vm_fd)
            result["vm_created"] = True
            # Intel requires a separate reserved three-page TSS region below 4GiB.
            stage = "set_tss_address"
            fcntl.ioctl(vm_fd, KVM_SET_TSS_ADDR, 0x100000)
            stage = "map_guest_memory"
            ram[:len(CODE)] = CODE
            address = ctypes.addressof(ctypes.c_char.from_buffer(ram))
            region = struct.pack("<IIQQQ", 0, 0, 0, RAM_BYTES, address)
            fcntl.ioctl(vm_fd, KVM_SET_USER_MEMORY_REGION, region)
            stage = "create_vcpu"
            vcpu_fd = fcntl.ioctl(vm_fd, KVM_CREATE_VCPU, 0)
            resources.callback(os.close, vcpu_fd)
            result["vcpu_created"] = True
            stage = "map_vcpu_state"
            size = fcntl.ioctl(kvm_fd, KVM_GET_VCPU_MMAP_SIZE, 0)
            require(RAM_BYTES <= size <= 1024 * 1024 and size % RAM_BYTES == 0, "unexpected KVM vCPU mapping size")
            run = resources.enter_context(mmap.mmap(vcpu_fd, size, flags=mmap.MAP_SHARED))
            stage = "initialize_registers"
            # kvm_sregs is 312 bytes on x86_64. Preserve the reset state, setting
            # only CS.base (offset 0) and CS.selector (offset 12) to address zero.
            sregs = bytearray(312)
            fcntl.ioctl(vcpu_fd, KVM_GET_SREGS, sregs, True)
            struct.pack_into("<Q", sregs, 0, 0)
            struct.pack_into("<H", sregs, 12, 0)
            fcntl.ioctl(vcpu_fd, KVM_SET_SREGS, sregs)
            # kvm_regs: 16 general registers, RIP=0, RFLAGS=reserved bit 1.
            fcntl.ioctl(vcpu_fd, KVM_SET_REGS, struct.pack("<18Q", *([0] * 17 + [2])))
            for expected in (KVM_EXIT_IO, KVM_EXIT_HLT):
                stage = "run_guest"
                fcntl.ioctl(vcpu_fd, KVM_RUN, 0)
                result["verified_exits"].append(verify_exit(run, expected))
            # Re-entry for HLT also commits the pending OUT operation.
        result["observed_byte"] = EXPECTED_BYTE
        result["status"] = "pass"
        return result, 0
    except VerificationFailed as exc:
        result.update(stage=stage, error=str(exc))
        return result, 1
    except OSError as exc:
        known = exc.errno in (errno.ENOENT, errno.ENODEV, errno.EACCES, errno.EPERM, errno.ENOSYS, errno.EOPNOTSUPP)
        result.update(stage=stage, status="fail" if known else "unknown", errno=exc.errno,
                      error=os.strerror(exc.errno) if exc.errno is not None else "operating system call failed")
        return result, 1 if known else 2
    except Exception as exc:
        result.update(stage=stage, status="unknown", error="unexpected runtime error: " + type(exc).__name__)
        return result, 2


if __name__ == "__main__":
    report, code = run_smoke(platform.system(), platform.machine())
    print(json.dumps(report, indent=2))
    sys.exit(code)
PY
)
smoke_status=$?
case "$smoke_status" in
    124|137)
        printf '%s\n' '{"schema_version":1,"status":"unknown","scope":"host_kvm_reference_only","native_vroom_x64_tested":false,"error":"KVM smoke exceeded its 10-second timeout (2-second kill grace)"}'
        exit 2
        ;;
    0|1|2)
        if [ -n "$smoke_output" ]; then
            printf '%s\n' "$smoke_output"
            exit "$smoke_status"
        fi
        printf '%s\n' '{"schema_version":1,"status":"unknown","scope":"host_kvm_reference_only","native_vroom_x64_tested":false,"error":"smoke runtime produced no result"}'
        exit 2
        ;;
    *)
        printf '%s\n' '{"schema_version":1,"status":"unknown","scope":"host_kvm_reference_only","native_vroom_x64_tested":false,"error":"smoke runtime terminated unexpectedly"}'
        exit 2
        ;;
esac

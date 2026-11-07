#!/bin/sh
# AL2023 x86_64 reference: packaged host kernel + generated minimal initramfs.
# No guest disks, NICs, host mounts, UEFI, TPM, or native vroom validation.
# Prerequisites: Python3, GNU timeout, rpm, packaged qemu-kvm-core and its BIOS.
# The caller must review the dnf transaction before installing prerequisites.
# This script installs/downloads nothing. At most 58s plus 2s kill grace.
set -u
if [ "$#" -ne 0 ] || ! command -v python3 >/dev/null 2>&1 || ! command -v timeout >/dev/null 2>&1; then
    printf '%s\n' '{"schema_version":1,"status":"unknown","scope":"linux_kernel_reference_only","native_vroom_x64_tested":false,"error":"requires python3 and GNU timeout; accepts no arguments"}'
    exit 2
fi
boot_output=$(timeout --signal=TERM --kill-after=2s 58s python3 - <<'PY'
import hashlib
import json
import os
import platform
import secrets
import selectors
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

OUTPUT_LIMIT = 128 * 1024


class CheckFailed(Exception):
    pass


def require(condition, message):
    if not condition:
        raise CheckFailed(message)


def init_elf(marker):
    """Small static x86_64 PID1: write marker, power off; exit(1) if that fails.

    Only Linux write(1), reboot(169) and exit(60) syscalls. No interpreter,
    libraries, filesystem operations or network. Built here, not downloaded.
    """
    message = b"\n" + marker.encode("ascii") + b"\n"
    code = bytearray(b"\xb8\x01\0\0\0\xbf\x01\0\0\0\x48\xbe" + bytes(8))
    code += b"\xba" + struct.pack("<I", len(message)) + b"\x0f\x05"
    code += b"\xb8" + struct.pack("<I", 169)  # reboot
    code += b"\xbf" + struct.pack("<I", 0xFEE1DEAD)
    code += b"\xbe" + struct.pack("<I", 0x28121969)
    code += b"\xba" + struct.pack("<I", 0x4321FEDC)  # POWER_OFF
    code += b"\x45\x31\xd2\x0f\x05"  # r10=0; syscall
    code += b"\xb8\x3c\0\0\0\xbf\x01\0\0\0\x0f\x05"  # exit(1)
    base, entry_offset = 0x400000, 120
    struct.pack_into("<Q", code, 12, base + entry_offset + len(code))
    size = entry_offset + len(code) + len(message)
    ident = b"\x7fELF\x02\x01\x01" + bytes(9)
    header = struct.pack("<16sHHIQQQIHHHHHH", ident, 2, 62, 1, base + entry_offset,
                         64, 0, 0, 64, 56, 1, 0, 0, 0)
    segment = struct.pack("<IIQQQQQQ", 1, 5, 0, base, base, size, size, 4096)
    return header + segment + code + message


def initramfs(marker):
    """Uncompressed newc archive: only /init and the guest console device."""
    archive = bytearray()
    entries = [
        (".", stat.S_IFDIR | 0o755, b"", 0, 0),
        ("dev", stat.S_IFDIR | 0o755, b"", 0, 0),
        ("dev/console", stat.S_IFCHR | 0o600, b"", 5, 1),
        ("init", stat.S_IFREG | 0o555, init_elf(marker), 0, 0),
        ("TRAILER!!!", 0, b"", 0, 0),
    ]
    for inode, (name, mode, data, major, minor) in enumerate(entries, 1):
        name = name.encode("ascii") + b"\0"
        fields = (inode, mode, 0, 0, 2 if stat.S_ISDIR(mode) else 1, 0, len(data),
                  0, 0, major, minor, len(name), 0)
        archive += b"070701" + b"".join(("%08x" % value).encode("ascii") for value in fields) + name
        archive += bytes(-len(archive) % 4)
        archive += data
        archive += bytes(-len(archive) % 4)
    return bytes(archive)


def command(qemu, kernel, ramfs, qmp):
    return [str(qemu), "-machine", "q35", "-accel", "kvm", "-cpu", "host",
            "-smp", "1", "-m", "512M", "-nodefaults", "-nic", "none",
            "-display", "none", "-monitor", "none", "-serial", "stdio",
            "-kernel", str(kernel), "-initrd", str(ramfs),
            "-append", "console=ttyS0 rdinit=/init quiet panic=-1",
            "-no-reboot", "-S", "-qmp", "unix:" + str(qmp) + ",server=on,wait=off"]


def accepted(kvm, output, marker, events, returncode):
    return (kvm.get("enabled") is True and kvm.get("present") is True
            and marker in output.decode("utf-8", "replace").splitlines()
            and any(event.get("event") == "SHUTDOWN"
                    and event.get("data", {}).get("guest") is True
                    and event.get("data", {}).get("reason") == "guest-shutdown" for event in events)
            and returncode == 0)


def package_owner(path, deadline):
    completed = subprocess.run(["rpm", "-qf", "--queryformat", "%{NAME}-%{VERSION}-%{RELEASE}.%{ARCH}", str(path)],
                               capture_output=True, text=True, timeout=min(5, max(0.1, deadline - time.monotonic())))
    require(completed.returncode == 0, "required artifact is not owned by an installed RPM")
    return completed.stdout.strip()


def run_boot():
    deadline = time.monotonic() + 55
    result = {"schema_version": 1, "scope": "linux_kernel_reference_only", "status": "unknown",
              "engine": "qemu_kvm", "guest_kind": "linux_kernel_minimal_initramfs", "guest_architecture": "x64",
              "native_vroom_x64_tested": False, "security_features_tested": False, "os_boot_verified": False,
              "guest_network_devices": 0, "guest_disk_devices": 0, "guest_host_mounts": 0,
              "limits": {"vcpus": 1, "guest_ram_mib": 512, "total_timeout_seconds": 60},
              "kvm": {}, "serial_marker_observed": False, "guest_poweroff_observed": False}
    output, qmp_buffer, events = bytearray(), bytearray(), []
    process = None
    stage = "host_preflight"
    try:
        require(platform.system() == "Linux" and platform.machine() == "x86_64", "requires Linux x86_64")
        require(Path("/dev/kvm").exists(), "/dev/kvm is absent; no software fallback is permitted")
        qemu = next((path for path in (Path("/usr/bin/qemu-system-x86_64"), Path("/usr/libexec/qemu-kvm"))
                     if path.is_file() and os.access(path, os.X_OK)), None)
        require(qemu is not None, "packaged QEMU missing; review qemu-kvm-core package transaction first")
        kernel = Path("/boot") / ("vmlinuz-" + platform.release())
        require(kernel.is_file() and kernel.stat().st_size <= 128 * 1024 * 1024, "packaged running kernel unavailable")
        stage = "verify_packages"
        result["qemu_package"] = package_owner(qemu, deadline)
        result["kernel_package"] = package_owner(kernel, deadline)
        require(result["qemu_package"].startswith("qemu-") and result["kernel_package"].startswith("kernel"),
                "unexpected RPM owners for QEMU/kernel")
        result["kernel_release"] = platform.release()
        result["kernel_sha256"] = hashlib.sha256(kernel.read_bytes()).hexdigest()
        marker = "VROOM_LINUX_BOOT_OK:" + secrets.token_hex(16)
        with tempfile.TemporaryDirectory(prefix="vroom-boot-") as temporary:
            directory = Path(temporary)
            ramfs, qmp_path = directory / "initramfs.cpio", directory / "qmp.sock"
            ramfs.write_bytes(initramfs(marker))
            stage = "start_qemu"
            # Keep QEMU in timeout's process group; it must die with the wrapper.
            process = subprocess.Popen(command(qemu, kernel, ramfs, qmp_path), stdin=subprocess.DEVNULL,
                                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
            try:
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as qmp, selectors.DefaultSelector() as selector:
                    stage = "connect_qmp"
                    connect_deadline = min(deadline, time.monotonic() + 5)
                    while True:
                        try:
                            qmp.connect(str(qmp_path))
                            break
                        except (FileNotFoundError, ConnectionRefusedError):
                            if process.poll() is not None:
                                output += process.stdout.read(OUTPUT_LIMIT + 1)
                                raise CheckFailed("QEMU exited before its local control socket became ready")
                            if time.monotonic() >= connect_deadline:
                                raise TimeoutError("QEMU control socket was not ready")
                            time.sleep(0.025)
                    qmp.setblocking(False)
                    selector.register(qmp, selectors.EVENT_READ, "qmp")
                    selector.register(process.stdout, selectors.EVENT_READ, "serial")

                    def send(name):
                        qmp.sendall(json.dumps({"execute": name, "id": name}).encode("ascii") + b"\n")

                    stage = "verify_kvm_and_boot"
                    while selector.get_map():
                        remaining = deadline - time.monotonic()
                        if remaining <= 0:
                            raise TimeoutError("Linux boot exceeded its deadline")
                        for key, _ in selector.select(min(0.1, remaining)):
                            data = os.read(key.fd, 8192)
                            if not data:
                                selector.unregister(key.fileobj)
                                continue
                            if key.data == "serial":
                                output += data
                                require(len(output) <= OUTPUT_LIMIT, "guest output exceeded its fixed limit")
                                continue
                            qmp_buffer += data
                            require(len(qmp_buffer) <= 16384, "QMP message exceeded its fixed limit")
                            while b"\n" in qmp_buffer:
                                line, _, rest = qmp_buffer.partition(b"\n")
                                qmp_buffer[:] = rest
                                message = json.loads(line)
                                require(isinstance(message, dict), "invalid QMP response")
                                require("error" not in message, "QEMU rejected a required control command")
                                if "QMP" in message:
                                    send("qmp_capabilities")
                                elif message.get("id") == "qmp_capabilities":
                                    send("query-kvm")
                                elif message.get("id") == "query-kvm":
                                    result["kvm"] = message.get("return", {})
                                    require(result["kvm"].get("enabled") is True and result["kvm"].get("present") is True,
                                            "QMP did not confirm active KVM acceleration")
                                    send("cont")
                                elif "event" in message:
                                    events.append(message)
                                    require(len(events) <= 64, "QMP event limit exceeded")
                    result["qemu_returncode"] = process.wait(timeout=max(0.1, deadline - time.monotonic()))
                    result["serial_marker_observed"] = marker in output.decode("utf-8", "replace").splitlines()
                    result["guest_poweroff_observed"] = any(event.get("event") == "SHUTDOWN"
                        and event.get("data", {}).get("guest") is True
                        and event.get("data", {}).get("reason") == "guest-shutdown" for event in events)
                    require(accepted(result["kvm"], output, marker, events, result["qemu_returncode"]),
                            "requires active KVM, guest PID1 serial marker, guest poweroff and clean QEMU exit")
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=1)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=1)
                process.stdout.close()
        result.update(status="pass", os_boot_verified=True)
        return result, 0
    except CheckFailed as exc:
        result.update(status="fail", stage=stage, error=str(exc))
        return result, 1
    except (OSError, TimeoutError, subprocess.TimeoutExpired) as exc:
        result.update(stage=stage, error=str(exc))
        return result, 2
    except Exception as exc:
        result.update(stage=stage, error="unexpected runtime error: " + type(exc).__name__)
        return result, 2
    finally:
        # Only this disposable guest's boot output; never host environment/keys.
        if result["status"] != "pass" and output:
            result["serial_excerpt"] = output[-4096:].decode("utf-8", "replace")


if __name__ == "__main__":
    report, code = run_boot()
    print(json.dumps(report, indent=2))
    sys.exit(code)
PY
)
boot_status=$?
case "$boot_status" in
    0|1|2)
        if [ -n "$boot_output" ]; then
            printf '%s\n' "$boot_output"
            exit "$boot_status"
        fi
        ;;
esac
printf '%s\n' '{"schema_version":1,"status":"unknown","scope":"linux_kernel_reference_only","native_vroom_x64_tested":false,"os_boot_verified":false,"error":"Linux boot workload timed out or terminated without a result (60-second bound)"}'
exit 2

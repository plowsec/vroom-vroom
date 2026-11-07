#!/usr/bin/env python3
"""Prepare a minimal, pinned, offline-buildable hypervisor smoke bundle. No AWS calls."""
import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import subprocess
import tarfile
import tomllib

ROOT = Path(__file__).resolve().parents[1]
CRATES = ("hv", "memory")
LINUX_CRATES = ("hv", "memory", "arch", "devices", "snapshot", "usb", "block", "linux-harness")
LINUX_SCRIPTS = ("scripts/make-linux-heartbeat.py", "scripts/tests/test_linux_heartbeat.py",
                 "scripts/select-packaged-kernel.py", "scripts/tests/test_packaged_kernel.py")
ARM_SCRIPTS = ("scripts/check-arm-kernel.py", "scripts/tests/test_arm_kernel.py")
GENERATED_FILES = ("Cargo.toml", "Cargo.lock", ".cargo/config.toml", "build-and-run.sh")
REGISTRY = "registry+https://github.com/rust-lang/crates.io-index"
MAX_UNPACKED = 64 * 1024 * 1024


class BundleError(ValueError):
    pass


def digest(data):
    return hashlib.sha256(data).hexdigest()


def toml_value(value):
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return json.dumps(value)
    if isinstance(value, (int, float)):
        return str(value)
    if isinstance(value, list):
        return "[" + ", ".join(toml_value(item) for item in value) + "]"
    if isinstance(value, dict):
        return "{ " + ", ".join(json.dumps(key) + " = " + toml_value(item) for key, item in value.items()) + " }"
    raise BundleError("unsupported TOML value")


def workload_crates(example):
    if example in ("linux_harness", "linux_harness_arm64"):
        return LINUX_CRATES
    if example in ("smoke", "smoke_x86_64"):
        return CRATES
    raise BundleError("only reviewed native smoke/harness workloads may be bundled")


def workload_target(example):
    workload_crates(example)
    return "aarch64-unknown-linux-gnu" if example in ("smoke", "linux_harness_arm64") else "x86_64-unknown-linux-gnu"


def minimal_manifest(root, example="smoke_x86_64"):
    crates = workload_crates(example)
    original = tomllib.loads((root / "Cargo.toml").read_text())
    needed = set()
    for crate in crates:
        crate_manifest = tomllib.loads((root / "crates" / crate / "Cargo.toml").read_text())
        for section in ("dependencies", "build-dependencies", "dev-dependencies"):
            for name, value in crate_manifest.get(section, {}).items():
                if isinstance(value, dict) and value.get("workspace"):
                    needed.add(name)
    dependencies = original["workspace"]["dependencies"]
    if any("path" in dependencies[name] and dependencies[name]["path"] not in {"crates/" + crate for crate in crates} for name in needed
           if isinstance(dependencies[name], dict)):
        raise BundleError("smoke gained another first-party dependency; review bundle scope")
    tables = {
        ("workspace",): {"resolver": "2", "members": ["crates/" + crate for crate in crates]},
        ("workspace", "package"): original["workspace"]["package"],
        ("workspace", "dependencies"): {name: dependencies[name] for name in sorted(needed)},
        ("workspace", "lints", "rust"): original["workspace"]["lints"]["rust"],
        ("workspace", "lints", "clippy"): original["workspace"]["lints"]["clippy"],
        ("profile", "dev"): {"panic": "abort"},
    }
    return "\n\n".join("[" + ".".join(json.dumps(key) for key in path) + "]\n" +
        "\n".join(json.dumps(key) + " = " + toml_value(value) for key, value in table.items())
        for path, table in tables.items()) + "\n"


def selected_sources(root, example):
    crates = workload_crates(example)
    excluded_backend = {"crates/hv/src/kvm/x86_64.rs"} if workload_target(example).startswith("aarch64") else {
        "crates/hv/src/kvm/aarch64.rs", "crates/hv/src/kvm/gic.rs"}
    selected = [Path("crates/hv/build.rs")]
    if example in ("linux_harness", "linux_harness_arm64"):
        selected.extend(Path(name) for name in LINUX_SCRIPTS)
        if example == "linux_harness_arm64":
            selected.extend(Path(name) for name in ARM_SCRIPTS)
            selected.append(Path("crates/hv/examples/smoke.rs"))
    else:
        selected.append(Path(f"crates/hv/examples/{example}.rs"))
    for crate in crates:
        selected.append(Path(f"crates/{crate}/Cargo.toml"))
        for path in sorted((root / "crates" / crate / "src").rglob("*.rs")):
            relative = path.relative_to(root)
            if crate == "hv" and ("hvf" in relative.parts or relative.as_posix() in excluded_backend):
                continue
            if relative.as_posix() == "crates/block/src/rollback.rs":
                continue  # gated research feature remains disabled in this workload
            selected.append(relative)
    for relative in selected:
        path = root / relative
        if not path.is_file() or path.is_symlink() or any(parent.is_symlink() for parent in path.parents if parent != root.parent):
            raise BundleError(f"source missing or symlinked: {relative}")
    return selected


def run_cargo(stage, *args):
    result = subprocess.run(["cargo", *args], cwd=stage, capture_output=True, text=True, timeout=300)
    if result.returncode:
        raise BundleError("cargo " + args[0] + " failed: " + result.stderr[-3000:])
    return result.stdout


def registry_packages(lock):
    return {(item["name"], item["version"], item.get("source"), item.get("checksum")) for item in lock["package"]}


def check_lock(original, reduced, example="smoke_x86_64"):
    allowed = registry_packages(original)
    for item in reduced["package"]:
        identity = (item["name"], item["version"], item.get("source"), item.get("checksum"))
        if identity not in allowed:
            raise BundleError(f"dependency version/checksum changed: {item['name']}")
        if item.get("source") not in (None, REGISTRY):
            raise BundleError("only pinned crates.io dependencies are allowed")
        if item.get("source") is None and item["name"] not in {"vroom-" + crate for crate in workload_crates(example)}:
            raise BundleError("unrelated workspace package leaked into reduced lockfile")


def stage_sources(root, stage, example, allow_fetch=False):
    if stage.exists():
        raise BundleError("stage directory already exists; choose a new empty path")
    sources = selected_sources(root, example)
    stage.mkdir(parents=True, mode=0o700)
    for relative in sources:
        destination = stage / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes((root / relative).read_bytes())
    (stage / "Cargo.toml").write_text(minimal_manifest(root, example))
    original = tomllib.loads((root / "Cargo.lock").read_text())
    shutil.copyfile(root / "Cargo.lock", stage / "Cargo.lock")
    # Cargo prunes the seeded lock; every retained version/checksum must remain pinned.
    network_flags = [] if allow_fetch else ["--offline"]
    run_cargo(stage, "metadata", *network_flags, "--format-version", "1", "--filter-platform", workload_target(example))
    reduced = tomllib.loads((stage / "Cargo.lock").read_text())
    check_lock(original, reduced, example)
    if allow_fetch:
        run_cargo(stage, "fetch", "--locked")
    run_cargo(stage, "vendor", "--frozen", "--versioned-dirs", "vendor")
    (stage / ".cargo").mkdir()
    (stage / ".cargo/config.toml").write_text('[source.crates-io]\nreplace-with = "vendored-sources"\n\n[source.vendored-sources]\ndirectory = "vendor"\n')
    (stage / "build-and-run.sh").write_text(build_script(example))
    os.chmod(stage / "build-and-run.sh", 0o755)
    manifest = {"schema_version": 1, "purpose": "native Linux harness only" if example in ("linux_harness", "linux_harness_arm64") else "native hypervisor smoke only", "example": example,
        "target": workload_target(example), "first_party_files": [str(path) for path in sorted(sources)],
        "registry_package_count": sum(item.get("source") is not None for item in reduced["package"]),
        "source_files": {str(path): digest((stage / path).read_bytes()) for path in sorted(sources)},
        "source_context_files": {name: digest((root / name).read_bytes()) for name in ("Cargo.toml", "Cargo.lock")},
        "generated_files": {name: digest((stage / name).read_bytes()) for name in GENERATED_FILES}}
    (stage / "SOURCE-MANIFEST.json").write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    return manifest


def build_script(example):
    if example == "linux_harness_arm64":
        return '''#!/bin/sh
set -eu
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = aarch64 ] || exit 2
cd "$(dirname "$0")"
exec timeout -k 5s 420s sh <<'VROOM_ARM_BOUND'
set -eu
export CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_HOME="$PWD/.cargo-home"
mkdir -p "$CARGO_HOME"
rustc --version
cargo --version
timeout -k 2s 15s python3 -I scripts/select-packaged-kernel.py --architecture arm64 --require-config > kernel-provenance.json
cat kernel-provenance.json
kernel=$(python3 -I -c 'import json; print(json.load(open("kernel-provenance.json"))["kernel"]["path"])')
config=$(python3 -I -c 'import json; print(json.load(open("kernel-provenance.json"))["config"]["path"])')
timeout -k 2s 10s python3 -I scripts/check-arm-kernel.py "$kernel" "$config"
timeout -k 5s 300s sh -c 'cargo test --frozen -p vroom-hv -p vroom-arch -p vroom-devices -p vroom-linux-harness --lib --target aarch64-unknown-linux-gnu && cargo build --frozen -p vroom-hv --example smoke --target aarch64-unknown-linux-gnu && cargo build --frozen -p vroom-linux-harness --target aarch64-unknown-linux-gnu'
timeout -k 2s 15s target/aarch64-unknown-linux-gnu/debug/examples/smoke
timeout -k 2s 8s python3 -m unittest discover -s scripts/tests -p test_linux_heartbeat.py
token=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
python3 scripts/make-linux-heartbeat.py --architecture arm64 --output heartbeat.cpio --token "$token"
sha256sum "$kernel" "$config" heartbeat.cpio
timeout -k 2s 70s target/aarch64-unknown-linux-gnu/debug/vroom-linux-harness "$kernel" heartbeat.cpio "$token"
VROOM_ARM_BOUND
'''
    if example == "linux_harness":
        return '''#!/bin/sh
set -eu
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || exit 2
cd "$(dirname "$0")"
export CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_HOME="$PWD/.cargo-home"
mkdir -p "$CARGO_HOME"
rustc --version
cargo --version
timeout -k 2s 15s python3 -I scripts/select-packaged-kernel.py --architecture x86_64 > kernel-provenance.json
cat kernel-provenance.json
kernel=$(python3 -I -c 'import json; print(json.load(open("kernel-provenance.json"))["kernel"]["path"])')
timeout -k 5s 300s sh -c 'cargo test --frozen -p vroom-hv -p vroom-arch -p vroom-devices -p vroom-linux-harness --lib --target x86_64-unknown-linux-gnu && cargo build --frozen -p vroom-linux-harness --target x86_64-unknown-linux-gnu'
timeout -k 2s 8s python3 -m unittest discover -s scripts/tests -p test_linux_heartbeat.py
token=$(python3 -c 'import secrets; print(secrets.token_hex(16))')
python3 scripts/make-linux-heartbeat.py --output heartbeat.cpio --token "$token"
sha256sum "$kernel" heartbeat.cpio
timeout -k 2s 70s target/x86_64-unknown-linux-gnu/debug/vroom-linux-harness "$kernel" heartbeat.cpio "$token"
'''
    target = workload_target(example)
    machine = "aarch64" if example == "smoke" else "x86_64"
    return f'''#!/bin/sh
set -eu
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = {machine} ] || exit 2
cd "$(dirname "$0")"
export CARGO_NET_OFFLINE=true CARGO_BUILD_JOBS=2 CARGO_HOME="$PWD/.cargo-home"
mkdir -p "$CARGO_HOME"
rustc --version
cargo --version
timeout -k 5s 180s sh -c 'cargo test --frozen -p vroom-hv --lib --target {target} && cargo build --frozen -p vroom-hv --example {example} --target {target}'
timeout -k 2s 15s target/{target}/debug/examples/{example}
'''


def bundle_files(stage):
    permitted = [stage / "Cargo.toml", stage / "Cargo.lock", stage / "SOURCE-MANIFEST.json",
                 stage / "build-and-run.sh", stage / ".cargo/config.toml"]
    manifest = json.loads((stage / "SOURCE-MANIFEST.json").read_text())
    example = manifest.get("example", "smoke_x86_64")
    allowed_prefixes = tuple("crates/" + crate + "/" for crate in workload_crates(example))
    for relative in manifest["first_party_files"]:
        path = PurePosixPath(relative)
        if path.is_absolute() or ".." in path.parts or not (relative.startswith(allowed_prefixes) or
                example in ("linux_harness", "linux_harness_arm64") and relative in LINUX_SCRIPTS or
                example == "linux_harness_arm64" and relative in ARM_SCRIPTS):
            raise BundleError("first-party inventory escapes the allowed crates")
        permitted.append(stage / relative)
    for directory in (stage / "vendor",):
        for path in sorted(directory.rglob("*")):
            if path.is_symlink():
                raise BundleError("bundle cannot contain symlinks")
            if path.is_file():
                permitted.append(path)
    for path in permitted:
        if not path.is_file() or path.is_symlink():
            raise BundleError("bundle file is missing or symlinked")
    if sum(path.stat().st_size for path in permitted) > MAX_UNPACKED:
        raise BundleError("bundle exceeds 64 MiB uncompressed limit")
    return sorted(permitted, key=lambda path: path.relative_to(stage).as_posix())


def pack(stage, destination, root=ROOT):
    source = json.loads((stage / "SOURCE-MANIFEST.json").read_text())
    for relative, expected in source["source_files"].items():
        if digest((root / relative).read_bytes()) != expected:
            raise BundleError("source changed since staging; restage after review")
        if digest((stage / relative).read_bytes()) != expected:
            raise BundleError("staged source changed since review")
    for relative, expected in source.get("source_context_files", {}).items():
        if digest((root / relative).read_bytes()) != expected:
            raise BundleError("source context changed since staging")
    if set(source.get("generated_files", {})) != set(GENERATED_FILES):
        raise BundleError("generated build inputs were not frozen; restage")
    for relative, expected in source["generated_files"].items():
        if digest((stage / relative).read_bytes()) != expected:
            raise BundleError("generated build input changed since staging")
    files = bundle_files(stage)
    inventory = {path.relative_to(stage).as_posix(): {"sha256": digest(path.read_bytes()), "size": path.stat().st_size}
                 for path in files}
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for path in files:
            data = path.read_bytes()
            info = tarfile.TarInfo(path.relative_to(stage).as_posix())
            info.size, info.mtime = len(data), 0
            info.mode = 0o755 if path.name == "build-and-run.sh" else 0o644
            archive.addfile(info, io.BytesIO(data))
        data = (json.dumps({"schema_version": 1, "files": inventory}, sort_keys=True) + "\n").encode()
        info = tarfile.TarInfo("BUNDLE-MANIFEST.json")
        info.size, info.mtime, info.mode = len(data), 0, 0o644
        archive.addfile(info, io.BytesIO(data))
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.exists():
        raise BundleError("destination already exists")
    with destination.open("xb") as stream, gzip.GzipFile(filename="", mode="wb", fileobj=stream, mtime=0) as compressed:
        compressed.write(raw.getvalue())
    checksum = digest(destination.read_bytes())
    destination.with_suffix(destination.suffix + ".sha256").write_text(checksum + "  " + destination.name + "\n")
    return {"path": str(destination.resolve()), "sha256": checksum, "bytes": destination.stat().st_size,
            "file_count": len(files) + 1, "unpacked_file_bytes": sum(item["size"] for item in inventory.values())}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    stage = sub.add_parser("stage")
    stage.add_argument("output", type=Path)
    stage.add_argument("--example", choices=("smoke", "smoke_x86_64", "linux_harness", "linux_harness_arm64"), default="smoke_x86_64")
    stage.add_argument("--allow-fetch", action="store_true")
    freeze = sub.add_parser("pack")
    freeze.add_argument("stage", type=Path)
    freeze.add_argument("output", type=Path)
    freeze.add_argument("--reviewed-source", action="store_true", required=True)
    args = parser.parse_args()
    if args.command == "stage":
        result = stage_sources(ROOT, args.output.resolve(), args.example, args.allow_fetch)
    else:
        result = pack(args.stage.resolve(), args.output.resolve())
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (BundleError, OSError, subprocess.TimeoutExpired) as error:
        raise SystemExit(str(error))

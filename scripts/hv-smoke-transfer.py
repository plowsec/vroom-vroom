#!/usr/bin/env python3
"""Generate bounded SSM payloads locally, or receive their hash-checked source chunks."""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import tarfile

BASE = Path("/var/lib/vroom-pilot")
CHUNK_BYTES = 18 * 1024
MAX_REQUEST_BYTES = 32 * 1024
MAX_ARCHIVE_BYTES = 12 * 1024 * 1024
MAX_EXTRACTED_BYTES = 64 * 1024 * 1024


class TransferError(ValueError):
    pass


def sha(data):
    return hashlib.sha256(data).hexdigest()


def validate(run_id, archive_hash):
    if not re.fullmatch(r"vroom-pilot-[a-f0-9]{12}", run_id) or not re.fullmatch(r"[a-f0-9]{64}", archive_hash):
        raise TransferError("invalid run identity or archive hash")


def owned_directory(run_id, archive_hash, base=BASE):
    validate(run_id, archive_hash)
    if base.is_symlink() or (base / "run-id").is_symlink() or (base / "run-id").read_text().strip() != run_id:
        raise TransferError("host run marker does not match transfer ownership")
    directory = base / run_id / archive_hash
    for path in (base / run_id, directory):
        if path.is_symlink():
            raise TransferError("transfer path cannot be a symlink")
        path.mkdir(mode=0o700, exist_ok=True)
    return directory


def receive(run_id, archive_hash, index, expected, encoded, base=BASE):
    if not 0 <= index <= 1023 or not re.fullmatch(r"[a-f0-9]{64}", expected):
        raise TransferError("invalid chunk index or hash")
    data = base64.b64decode(encoded, validate=True)
    if not data or len(data) > CHUNK_BYTES or sha(data) != expected:
        raise TransferError("chunk size/hash does not match")
    directory = owned_directory(run_id, archive_hash, base)
    destination = directory / (f"{index:04d}.part")
    if destination.is_symlink():
        raise TransferError("chunk path cannot be a symlink")
    if destination.exists():
        if destination.read_bytes() != data:
            raise TransferError("existing chunk differs; refusing overwrite")
    else:
        with destination.open("xb") as output:
            output.write(data)
    return {"index": index, "bytes": len(data), "sha256": expected, "verified": True}


def safe_members(archive):
    members = archive.getmembers()
    if len(members) > 20000 or any(item.size < 0 for item in members) or sum(item.size for item in members) > MAX_EXTRACTED_BYTES:
        raise TransferError("archive exceeds extraction limits")
    seen = set()
    for item in members:
        path = PurePosixPath(item.name)
        if (not item.isfile() or path.is_absolute() or ".." in path.parts or "\\" in item.name
                or not path.parts or path.as_posix() != item.name or item.name in seen):
            raise TransferError("archive contains unsafe path, duplicate, or nonregular member")
        seen.add(item.name)
    return members


def verify_tree(directory, expected_manifest):
    manifest_path = directory / "BUNDLE-MANIFEST.json"
    if manifest_path.is_symlink() or manifest_path.read_bytes() != expected_manifest:
        raise TransferError("extracted manifest differs from verified archive")
    manifest = json.loads(expected_manifest)
    actual = {path.relative_to(directory).as_posix() for path in directory.rglob("*") if path.is_file()}
    if actual != set(manifest["files"]) | {"BUNDLE-MANIFEST.json"}:
        raise TransferError("extracted source contains missing or unexpected files")
    for relative, expected in manifest["files"].items():
        path = PurePosixPath(relative)
        if path.is_absolute() or ".." in path.parts:
            raise TransferError("manifest path escapes source directory")
        source = directory / relative
        if source.is_symlink() or not source.is_file() or directory.resolve() not in source.resolve().parents:
            raise TransferError("source file missing or symlinked")
        data = source.read_bytes()
        if len(data) != expected["size"] or sha(data) != expected["sha256"]:
            raise TransferError("source content hash differs")
    return len(manifest["files"])


def extract(run_id, archive_hash, parts, size, base=BASE):
    if not 1 <= parts <= 1024 or not 1 <= size <= MAX_ARCHIVE_BYTES:
        raise TransferError("invalid bounded transfer dimensions")
    directory = owned_directory(run_id, archive_hash, base)
    data = bytearray()
    for index in range(parts):
        path = directory / (f"{index:04d}.part")
        if path.is_symlink() or not path.is_file():
            raise TransferError("missing or symlinked chunk")
        chunk = path.read_bytes()
        if not chunk or len(chunk) > CHUNK_BYTES:
            raise TransferError("invalid stored chunk length")
        data.extend(chunk)
        if len(data) > MAX_ARCHIVE_BYTES:
            raise TransferError("stored transfer exceeds limit")
    if len(data) != size or sha(data) != archive_hash:
        raise TransferError("assembled archive size/hash differs")
    archive_path = directory / "source.tar.gz"
    if archive_path.is_symlink():
        raise TransferError("archive path cannot be a symlink")
    archive_path.write_bytes(data)
    destination = directory / "source"
    if destination.is_symlink():
        raise TransferError("source directory cannot be a symlink")
    with tarfile.open(archive_path, "r:gz") as archive:
        members = safe_members(archive)
        expected_manifest = archive.extractfile("BUNDLE-MANIFEST.json").read()
        if set(json.loads(expected_manifest)["files"]) != {item.name for item in members} - {"BUNDLE-MANIFEST.json"}:
            raise TransferError("archive inventory is incomplete")
        if not destination.exists():
            destination.mkdir(mode=0o700)
            for member in members:
                path = destination / member.name
                path.parent.mkdir(parents=True, exist_ok=True)
                with path.open("xb") as output, archive.extractfile(member) as source:
                    shutil.copyfileobj(source, output)
                os.chmod(path, 0o755 if member.name == "build-and-run.sh" else 0o644)
    files = verify_tree(destination, expected_manifest)
    return {"run_id": run_id, "archive_sha256": archive_hash, "source_path": str(destination),
            "verified_files": files, "verified": True}


def request(instance_id, comment, command):
    payload = {"InstanceIds": [instance_id], "DocumentName": "AWS-RunShellScript", "Comment": comment,
        "TimeoutSeconds": 120, "Parameters": {"commands": [command], "executionTimeout": ["120"]}}
    encoded = json.dumps(payload, separators=(",", ":"))
    if len(encoded.encode()) > MAX_REQUEST_BYTES:
        raise TransferError("SSM request exceeds conservative 32 KiB limit")
    return encoded


def generate(archive, run_id, instance_id, output):
    if not re.fullmatch(r"i-[a-f0-9]{17}", instance_id):
        raise TransferError("one concrete EC2 instance ID is required")
    data = archive.read_bytes()
    archive_hash = sha(data)
    validate(run_id, archive_hash)
    if not 1 <= len(data) <= MAX_ARCHIVE_BYTES or output.exists():
        raise TransferError("archive too large/empty or output already exists")
    with tarfile.open(archive, "r:gz") as opened:
        safe_members(opened)
    output.mkdir(parents=True, mode=0o700)
    helper = Path(__file__).read_bytes()
    helper_hash = sha(helper)
    helper_path = str(BASE / run_id / "transfer.py")
    bootstrap = "\n".join(["python3 - <<'PY'", "import base64,hashlib,pathlib,os",
        f"base=pathlib.Path({str(BASE)!r}); run={run_id!r}",
        "assert not base.is_symlink() and not (base/'run-id').is_symlink()",
        "assert (base/'run-id').read_text().strip()==run",
        "dest=base/run; assert not dest.is_symlink(); dest.mkdir(mode=0o700,exist_ok=True)",
        f"data=base64.b64decode({base64.b64encode(helper).decode()!r}); assert hashlib.sha256(data).hexdigest()=={helper_hash!r}",
        "path=dest/'transfer.py'; assert not path.is_symlink(); path.write_bytes(data); os.chmod(path,0o700)",
        "print('transfer helper verified')", "PY"])
    requests = [("0000-bootstrap.json", request(instance_id, run_id + " source helper", bootstrap))]
    chunks = [data[index:index + CHUNK_BYTES] for index in range(0, len(data), CHUNK_BYTES)]
    prefix = f"echo '{helper_hash}  {helper_path}' | sha256sum -c - >/dev/null && python3 {helper_path}"
    for index, chunk in enumerate(chunks):
        command = (f"{prefix} put --run-id {run_id} --archive-sha256 {archive_hash} --index {index} "
                   f"--chunk-sha256 {sha(chunk)} --data {base64.b64encode(chunk).decode()}")
        requests.append((f"{index + 1:04d}-chunk.json", request(instance_id, f"{run_id} source part {index}", command)))
    command = f"{prefix} extract --run-id {run_id} --archive-sha256 {archive_hash} --parts {len(chunks)} --bytes {len(data)}"
    requests.append((f"{len(chunks) + 1:04d}-extract.json", request(instance_id, run_id + " source verify", command)))
    for name, body in requests:
        (output / name).write_text(body + "\n")
    plan = {"run_id": run_id, "instance_id": instance_id, "archive_sha256": archive_hash,
        "archive_bytes": len(data), "chunk_count": len(chunks), "request_count": len(requests),
        "maximum_request_bytes": max(len(body.encode()) for _, body in requests),
        "ordered_requests": [name for name, _ in requests], "helper_sha256": helper_hash,
        "source_path": str(BASE / run_id / archive_hash / "source")}
    (output / "transfer-plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    return plan


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    generate_parser = sub.add_parser("generate", help="write SSM request JSON locally; never sends it")
    generate_parser.add_argument("archive", type=Path)
    generate_parser.add_argument("output", type=Path)
    generate_parser.add_argument("--run-id", required=True)
    generate_parser.add_argument("--instance-id", required=True)
    for name in ("put", "extract"):
        command = sub.add_parser(name)
        command.add_argument("--run-id", required=True)
        command.add_argument("--archive-sha256", required=True)
        if name == "put":
            command.add_argument("--index", type=int, required=True)
            command.add_argument("--chunk-sha256", required=True)
            command.add_argument("--data", required=True)
        else:
            command.add_argument("--parts", type=int, required=True)
            command.add_argument("--bytes", type=int, required=True)
    args = parser.parse_args()
    if args.command == "generate":
        result = generate(args.archive, args.run_id, args.instance_id, args.output)
    elif args.command == "put":
        result = receive(args.run_id, args.archive_sha256, args.index, args.chunk_sha256, args.data)
    else:
        result = extract(args.run_id, args.archive_sha256, args.parts, args.bytes)
    print(json.dumps(result, indent=2))


if __name__ == "__main__":
    try:
        main()
    except (TransferError, OSError, ValueError, tarfile.TarError) as error:
        raise SystemExit(str(error))

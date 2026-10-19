#!/usr/bin/env python3
"""Make VMs made by older vroom builds movable: their own records (disk
overlays' backing names, disk pointers, snapshots, suspend images) name the
files inside the VM's folder relative to where the record is, as vroom
writes them now. Files outside the folder keep their absolute paths. Disk
contents and modification times stay as they are (snapshots check them).

  scripts/relative-paths.py [VM.toml ...]          what would change
  scripts/relative-paths.py --apply [VM.toml ...]  change it, log for undo
  scripts/relative-paths.py --undo LOG             put it back

Without VM files: every VM in vroom's library. A VM that runs (its state
dir locked) is left alone: stop it and run again.
"""
import fcntl
import json
import os
import stat
import struct
import sys
import time
import tomllib

QCOW2 = b"QFI\xfb"


def data_dir():
    if os.environ.get("VROOM_HOME"):
        return os.environ["VROOM_HOME"]
    home = os.path.expanduser("~")
    if sys.platform == "darwin":
        return os.path.join(home, "Library/Application Support/vroom")
    return os.path.join(os.environ.get("XDG_DATA_HOME", os.path.join(home, ".local/share")), "vroom")


def library():
    try:
        with open(os.path.join(data_dir(), "library.toml"), "rb") as f:
            return tomllib.load(f).get("vms", [])
    except FileNotFoundError:
        return []


def inside(p, here, vm):
    """What a record in directory `here` should say for `p`, if it should
    change: a file in the VM folder, relative to `here`. Relative paths
    are taken as relative to `here`, or else (written by a build between)
    to the VM folder."""
    if not p:
        return None
    here = os.path.realpath(here)
    if os.path.isabs(p):
        target = p
    elif os.path.exists(os.path.join(here, p)):
        return None
    else:
        target = os.path.join(vm, p)
    if not os.path.exists(target):
        return None
    r = os.path.realpath(target)
    if not r.startswith(vm + "/") or not here.startswith(vm):
        return None
    new = os.path.relpath(r, here)
    return new if new != p else None


def backing_name(image, backing):
    """vroom's rule: relative to the overlay's directory when the backing
    file is at most two directories up."""
    here = os.path.realpath(os.path.dirname(image))
    to = os.path.realpath(backing)
    up = here
    for _ in range(3):
        if to.startswith(up + "/"):
            return os.path.relpath(to, here)
        up = os.path.dirname(up)
    return None


def backing_field(f):
    """(offset, length, name) of a qcow2 image's backing name, or None."""
    with open(f, "rb") as h:
        b = h.read(104)
        if len(b) < 104 or b[:4] != QCOW2:
            return None
        off, ln = struct.unpack(">Q", b[8:16])[0], struct.unpack(">I", b[16:20])[0]
        cluster = 1 << struct.unpack(">I", b[20:24])[0]
        if not off or not ln or off + ln > cluster:
            return None
        h.seek(off)
        return off, ln, h.read(ln).decode()


def set_backing(f, off, room, name):
    """Write `name` where the old one was (`room` bytes), keeping the file's
    mode and times."""
    st = os.stat(f)
    mode = stat.S_IMODE(st.st_mode)
    if len(name) > room:
        raise ValueError(f"{f}: {name!r} doesn't fit")
    if not mode & 0o200:
        os.chmod(f, mode | 0o200)
    try:
        with open(f, "r+b") as h:
            h.seek(off)
            h.write(name.encode() + b"\0" * (room - len(name)))
            h.seek(16)
            h.write(struct.pack(">I", len(name)))
            h.flush()
            os.fsync(h.fileno())
    finally:
        os.utime(f, ns=(st.st_atime_ns, st.st_mtime_ns))
        os.chmod(f, mode)


def replace_text(f, text):
    tmp = f + ".tmp"
    with open(tmp, "w") as h:
        h.write(text)
    os.chmod(tmp, stat.S_IMODE(os.stat(f).st_mode))
    os.replace(tmp, f)


class Vm:
    def __init__(self, toml, apply):
        self.dir = os.path.realpath(os.path.dirname(os.path.abspath(toml)))
        self.apply = apply
        self.undo = []

    def states(self):
        return sorted(os.path.join(self.dir, d) for d in os.listdir(self.dir) if d.endswith(".state"))

    def lock(self):
        """Hold every state dir's lock while we change things; None if a VM
        runs from one."""
        held = []
        for s in self.states():
            p = os.path.join(s, "lock")
            if not os.path.exists(p):
                continue
            fd = os.open(p, os.O_RDWR | os.O_NOFOLLOW | os.O_NONBLOCK)
            try:
                fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                os.close(fd)
                for h in held:
                    os.close(h)
                return None
            held.append(fd)
        return held

    def say(self, what):
        print(f"  {what}")

    def overlay(self, f):
        field = backing_field(f)
        if not field:
            return
        off, ln, old = field
        if not os.path.isabs(old) or not os.path.exists(old):
            return
        new = backing_name(f, old)
        if not new:
            return
        self.say(f"{os.path.relpath(f, self.dir)}: backing {old} -> {new}")
        self.undo.append({"overlay": f, "offset": off, "room": ln, "name": old})
        if self.apply:
            set_backing(f, off, ln, new)

    def text(self, f, old, new):
        self.say(os.path.relpath(f, self.dir))
        self.undo.append({"file": f, "text": old})
        if self.apply:
            replace_text(f, new)

    def pointer(self, f):
        old = open(f).read()
        d = json.loads(old)
        r = inside(d.get("active"), os.path.dirname(f), self.dir)
        if r:
            d["active"] = r
            self.text(f, old, json.dumps(d, separators=(",", ":")))

    def manifest(self, f):
        old = open(f).read()
        m = json.loads(old)
        changed = False

        here = os.path.dirname(f)

        def fix(obj, key):
            nonlocal changed
            r = inside(obj.get(key), here, self.dir) if isinstance(obj, dict) else None
            if r:
                obj[key] = r
                changed = True

        c = m.get("config") or {}
        for d in c.get("disk", []):
            fix(d, "path")
        for s in c.get("share", []):
            fix(s, "path")
        for k in ("vars", "firmware", "kernel", "initrd"):
            fix(c.get("boot"), k)
        fix(c.get("serial"), "path")
        meta = m.get("meta", {})
        for k, v in list(meta.items()):
            if k.startswith("disk."):
                fix(meta, k)
            elif k.startswith("chain."):
                lines = []
                for line in v.splitlines():
                    stamp, p = line.split("|", 1)
                    r = inside(p, here, self.dir)
                    changed |= bool(r)
                    lines.append(f"{stamp}|{r or p}")
                meta[k] = "\n".join(lines)
        if changed:
            self.text(f, old, json.dumps(m, indent=2))

    def run(self):
        for s in self.states():
            disks = os.path.join(s, "disks")
            for n in sorted(os.listdir(disks)) if os.path.isdir(disks) else []:
                p = os.path.join(disks, n)
                if n.endswith(".qcow2"):
                    self.overlay(p)
                elif n.endswith(".json"):
                    self.pointer(p)
        for root, _, files in os.walk(self.dir):
            if "manifest.json" in files:
                self.manifest(os.path.join(root, "manifest.json"))


def undo(log):
    for e in reversed(json.load(open(log))):
        if "overlay" in e:
            set_backing(e["overlay"], e["offset"], e["room"], e["name"])
        else:
            replace_text(e["file"], e["text"])
        print(f"  restored {e.get('overlay') or e['file']}")


def main():
    args = sys.argv[1:]
    if "--undo" in args:
        undo(args[args.index("--undo") + 1])
        return
    apply = "--apply" in args
    vms = [a for a in args if a != "--apply"] or library()
    if not vms:
        sys.exit("no VM files given and none in vroom's library")
    log = []
    for toml in vms:
        if not os.path.isfile(toml):
            print(f"== {toml}: not found, skipped")
            continue
        vm = Vm(toml, apply)
        held = vm.lock()
        if held is None:
            print(f"== {vm.dir}: running, skipped (stop it and run again)")
            continue
        print(f"== {vm.dir}")
        try:
            vm.run()
        finally:
            for fd in held:
                os.close(fd)
        log += vm.undo
    if not log:
        print("nothing to change")
    elif apply:
        path = os.path.join(data_dir(), time.strftime("relative-paths-undo-%Y%m%d-%H%M%S.json"))
        with open(path, "w") as h:
            json.dump(log, h, indent=1)
        print(f"{len(log)} changes; undo: scripts/relative-paths.py --undo '{path}'")
    else:
        print(f"{len(log)} changes; run with --apply to make them")


if __name__ == "__main__":
    main()

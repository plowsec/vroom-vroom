#!/usr/bin/env python3
"""Boot a Linux guest under vroom (or QEMU, for comparison) and measure it.

usage: bench.py --table RESULTS.jsonl   (medians per tag, Markdown)
       bench.py WORKDIR [--vmm vroom|qemu] [--bin PATH] [--boot kernel|uefi]
                [--bus virtio|nvme] [--fmt raw|qcow2] [--tests boot,idle,exit,fio,net]
                [--runs N] [--tag NAME]

WORKDIR holds (see docs/PERFORMANCE.md for how to make them):
  root.qcow2   Ubuntu cloud image with fio and iperf3 installed
  seed.iso     cloud-init seed that puts id_bench.pub in ubuntu's authorized_keys
  id_bench     the matching ssh key
  vmlinuz, initrd (direct kernel boot)
A fresh 2 GiB data disk (data.raw / data.qcow2) is made for every run and
deleted after it; fio only touches that disk. Results go to stdout as a table and are appended
to WORKDIR/results.jsonl.

What is measured:
  boot  seconds from starting the VMM to "login:" on the serial console
  idle  host CPU % of the VMM process while the guest sits at the prompt
        (20 s, after 10 s settling), and its RSS
  exit  microseconds per PCI config read in the guest (one MMIO exit and
        a syscall), and the round trip of a pipe ping-pong between two
        vCPUs (two cross-vCPU wakeups: IPIs to an idle vCPU)
  fio   4k random read/write and 1M sequential read/write at queue depth 1
        and 32 (libaio, O_DIRECT) on the data disk
  net   iperf3 TCP both ways and UDP guest->host through NAT (to the host's
        loopback via 10.0.2.2), TCP both ways through a port forward, ping
        to the gateway, and the median round trip of a 1-byte TCP
        ping-pong through a port forward
"""
import argparse, json, os, re, shutil, signal, socket, subprocess, sys, threading, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
QEMU = "/opt/homebrew/bin/qemu-system-aarch64"
EDK2 = "/opt/homebrew/share/qemu/edk2-aarch64-code.fd"
EDK2_VARS = "/opt/homebrew/share/qemu/edk2-arm-vars.fd"
CMDLINE = "console=ttyAMA0 root=LABEL=cloudimg-rootfs ro"
DATA_SIZE = 2 << 30

FIO_JOBS = [
    # name, rw, bs, iodepth
    ("randread-4k-qd1", "randread", "4k", 1),
    ("randread-4k-qd32", "randread", "4k", 32),
    ("randwrite-4k-qd1", "randwrite", "4k", 1),
    ("randwrite-4k-qd32", "randwrite", "4k", 32),
    ("read-1m-qd1", "read", "1m", 1),
    ("read-1m-qd32", "read", "1m", 32),
    ("write-1m-qd1", "write", "1m", 1),
    ("write-1m-qd32", "write", "1m", 32),
]


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


class Vm:
    def __init__(self, a, run_dir):
        self.a, self.dir = a, run_dir
        self.ssh_port, self.fwd_port, self.rr_port = free_port(), free_port(), free_port()
        self.login_at = None
        self.serial = open(os.path.join(run_dir, "serial.log"), "wb")

    def forwards(self):
        return [(self.ssh_port, 22), (self.fwd_port, 5201), (self.rr_port, 5202)]

    def make_data_disk(self):
        if self.a.fmt == "raw":
            p = os.path.join(self.dir, "data.raw")
            # Real blocks, not a hole: reads of holes never touch storage.
            with open(p, "wb") as f:
                chunk = os.urandom(1 << 20)
                for _ in range(DATA_SIZE >> 20):
                    f.write(chunk)
        else:
            p = os.path.join(self.dir, "data.qcow2")
            subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", p, str(DATA_SIZE)], check=True)
        return p

    def vroom_cmd(self, data):
        w = self.a.workdir
        cfg = [f'name = "bench"', "cpus = 4", "memory_mib = 4096", 'virtio_transport = "pci"']
        if self.a.boot == "kernel":
            cfg += ["[boot]", 'type = "kernel"', f'kernel = "{w}/vmlinuz"', f'initrd = "{w}/initrd"', f'cmdline = "{CMDLINE}"']
        else:
            cfg += ["[boot]", 'type = "uefi"', f'vars = "{w}/vroom-vars.fd"']
        cfg += ["[[disk]]", f'path = "{w}/root.qcow2"', 'format = "qcow2"']
        cfg += ["[[disk]]", f'path = "{w}/seed.iso"', "readonly = true", 'format = "raw"']
        if data:
            cfg += ["[[disk]]", f'path = "{data}"', f'format = "{self.a.fmt}"', f'bus = "{self.a.bus}"']
        fwds = ", ".join(f"{{ host_port = {h}, guest_port = {g} }}" for h, g in self.forwards())
        cfg += ["[[net]]", 'mode = "nat"', "allow_host_loopback = true", f"port_forwards = [{fwds}]"]
        p = os.path.join(self.dir, "vm.toml")
        with open(p, "w") as f:
            f.write("\n".join(cfg) + "\n")
        return [self.a.bin, "run", p, "--state-dir", os.path.join(self.dir, "state"), "--fresh"]

    def qemu_cmd(self, data):
        w = self.a.workdir
        c = [self.a.bin, "-accel", "hvf", "-M", "virt", "-cpu", "host", "-smp", "4", "-m", "4096",
             "-display", "none", "-serial", "stdio", "-monitor", "none", "-nodefaults"]
        if self.a.boot == "kernel":
            c += ["-kernel", f"{w}/vmlinuz", "-initrd", f"{w}/initrd", "-append", CMDLINE]
        else:
            vars_ = f"{w}/qemu-vars.fd"
            if not os.path.exists(vars_):
                shutil.copy(EDK2_VARS, vars_)
            c += ["-drive", f"if=pflash,format=raw,readonly=on,file={EDK2}", "-drive", f"if=pflash,format=raw,file={vars_}"]
        c += ["-drive", f"file={w}/root.qcow2,if=none,id=d0,format=qcow2", "-device", "virtio-blk-pci,drive=d0"]
        c += ["-drive", f"file={w}/seed.iso,if=none,id=d1,format=raw,readonly=on", "-device", "virtio-blk-pci,drive=d1"]
        if data:
            c += ["-drive", f"file={data},if=none,id=d2,format={self.a.fmt}"]
            c += ["-device", "nvme,drive=d2,serial=bench" if self.a.bus == "nvme" else "virtio-blk-pci,drive=d2"]
        fwds = "".join(f",hostfwd=tcp:127.0.0.1:{h}-:{g}" for h, g in self.forwards())
        c += ["-netdev", f"user,id=n0{fwds}",
              "-device", "virtio-net-pci,netdev=n0"]
        return c

    def start(self, with_data):
        data = self.make_data_disk() if with_data else None
        cmd = self.vroom_cmd(data) if self.a.vmm == "vroom" else self.qemu_cmd(data)
        self.t0 = time.monotonic()
        self.p = subprocess.Popen(cmd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        tail = b""
        while True:
            d = self.p.stdout.read1(65536)
            if not d:
                return
            self.serial.write(d)
            self.serial.flush()
            tail = (tail + d)[-256:]
            if self.login_at is None and b"login:" in tail:
                self.login_at = time.monotonic() - self.t0

    def wait_login(self, timeout=300):
        end = time.monotonic() + timeout
        while self.login_at is None:
            if self.p.poll() is not None or time.monotonic() > end:
                raise RuntimeError("no login prompt (see serial.log)")
            time.sleep(0.05)
        return self.login_at

    def ssh(self, cmd, check=True, timeout=600):
        r = subprocess.run(["ssh", "-q", "-i", os.path.join(self.a.workdir, "id_bench"), "-o", "StrictHostKeyChecking=no",
                            "-o", "UserKnownHostsFile=/dev/null", "-o", "ConnectTimeout=5", "-p", str(self.ssh_port),
                            "ubuntu@127.0.0.1", cmd], capture_output=True, text=True, timeout=timeout)
        if check and r.returncode != 0:
            raise RuntimeError(f"ssh {cmd!r}: {r.stderr.strip()}")
        return r.stdout

    def wait_ssh(self, timeout=180):
        end = time.monotonic() + timeout
        while time.monotonic() < end:
            try:
                if self.ssh("true", check=False, timeout=15) is not None and self.ssh("echo ok", check=False, timeout=15).strip() == "ok":
                    return
            except subprocess.TimeoutExpired:
                pass
            time.sleep(1)
        raise RuntimeError("ssh never came up")

    def cpu_seconds(self):
        out = subprocess.run(["ps", "-o", "cputime=", "-p", str(self.p.pid)], capture_output=True, text=True).stdout.strip()
        # [[dd-]hh:]mm:ss.cc
        secs = 0.0
        for part in out.split(":"):
            secs = secs * 60 + float(part.split("-")[-1])
        return secs

    def rss_mib(self):
        out = subprocess.run(["ps", "-o", "rss=", "-p", str(self.p.pid)], capture_output=True, text=True).stdout.strip()
        return int(out) / 1024

    def stop(self):
        try:
            self.ssh("sudo systemctl poweroff", check=False, timeout=15)
        except subprocess.TimeoutExpired:
            pass
        try:
            self.p.wait(60)
        except subprocess.TimeoutExpired:
            self.p.send_signal(signal.SIGKILL)
            self.p.wait()


def data_dev(vm):
    # The 2 GiB disk, whatever the guest named it.
    for line in vm.ssh("lsblk -bdno NAME,SIZE").splitlines():
        name, size = line.split()
        if int(size) == DATA_SIZE:
            return "/dev/" + name
    raise RuntimeError("data disk not found")


def run_fio(vm, runtime, only=None):
    dev = data_dev(vm)
    # Fill the disk once: a fresh qcow2 allocates here, so later runs
    # measure steady state, as on a disk in use.
    vm.ssh(f"sudo fio --name=fill --filename={dev} --rw=write --bs=1m --iodepth=32 --ioengine=libaio --direct=1 --output-format=json >/dev/null")
    res = {}
    for name, rw, bs, qd in FIO_JOBS:
        if only and not re.search(only, name):
            continue
        out = vm.ssh(f"sudo fio --name={name} --filename={dev} --rw={rw} --bs={bs} --iodepth={qd} --ioengine=libaio "
                     f"--direct=1 --time_based --runtime={runtime} --ramp_time=1 --output-format=json")
        j = json.loads(out[out.index("{"):])["jobs"][0]
        side = j["read"] if "read" in rw else j["write"]
        res[name] = {"iops": round(side["iops"]), "mbs": round(side["bw_bytes"] / 1e6),
                     "lat_us": round(side["clat_ns"]["mean"] / 1000, 1)}
    return res


def iperf_guest(vm, args):
    out = vm.ssh(f"iperf3 -J {args}", check=False)
    j = json.loads(out[out.index("{"):])
    if "error" in j:
        raise RuntimeError(f"iperf3 {args}: {j['error']}")
    return j


ECHO_SERVER = ("python3 -c 'import socket\n"
               "s=socket.socket();s.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1);s.bind((\"0.0.0.0\",5202));s.listen()\n"
               "c,_=s.accept();c.setsockopt(socket.IPPROTO_TCP,socket.TCP_NODELAY,1)\n"
               "while (d:=c.recv(64)): c.sendall(d)' >/dev/null 2>&1 &")


def tcp_rr_us(port, n=2000):
    """Median round trip of a 1-byte TCP ping-pong through the port forward."""
    for _ in range(50):
        try:
            c = socket.create_connection(("127.0.0.1", port), timeout=5)
            break
        except OSError:
            time.sleep(0.1)
    c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    t = []
    for _ in range(n):
        t0 = time.perf_counter()
        c.sendall(b"x")
        if not c.recv(1):
            raise RuntimeError("echo server closed")
        t.append(time.perf_counter() - t0)
    c.close()
    t.sort()
    return round(t[len(t) // 2] * 1e6)


def iperf_host(port, secs, *extra):
    r = subprocess.run(["iperf3", "-J", "-c", "127.0.0.1", "-p", str(port), "-t", str(secs), *extra],
                       capture_output=True, text=True)
    j = json.loads(r.stdout)
    if "error" in j:
        raise RuntimeError(f"iperf3: {j['error']}")
    return j


def run_net(vm, secs):
    # Guest -> host goes to 10.0.2.2, the NAT's alias for the host's
    # loopback (vroom: allow_host_loopback; QEMU's slirp does it by default).
    res = {}
    srv = subprocess.Popen(["iperf3", "-s", "-B", "127.0.0.1", "-p", "5201"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        time.sleep(0.5)
        j = iperf_guest(vm, f"-c 10.0.2.2 -t {secs}")
        res["tcp_g2h_gbit"] = round(j["end"]["sum_received"]["bits_per_second"] / 1e9, 2)
        j = iperf_guest(vm, f"-c 10.0.2.2 -t {secs} -R")
        res["tcp_h2g_gbit"] = round(j["end"]["sum_received"]["bits_per_second"] / 1e9, 2)
        j = iperf_guest(vm, f"-u -b 3G -l 1400 -c 10.0.2.2 -t {secs}")
        s = j["end"]["sum"]
        res["udp_g2h_gbit"] = round(s["bits_per_second"] * (1 - s.get("lost_percent", 0) / 100) / 1e9, 2)
    finally:
        srv.terminate()
        srv.wait()
    out = vm.ssh("ping -q -c 50 -i 0.05 10.0.2.2", check=False)
    m = re.search(r"= [\d.]+/([\d.]+)/", out)
    res["ping_gw_ms"] = float(m.group(1)) if m else None
    vm.ssh("iperf3 -s -D; " + ECHO_SERVER, check=False)
    time.sleep(0.5)
    j = iperf_host(vm.fwd_port, secs)
    res["fwd_h2g_gbit"] = round(j["end"]["sum_received"]["bits_per_second"] / 1e9, 2)
    j = iperf_host(vm.fwd_port, secs, "-R")
    res["fwd_g2h_gbit"] = round(j["end"]["sum_received"]["bits_per_second"] / 1e9, 2)
    res["fwd_rr_us"] = tcp_rr_us(vm.rr_port)
    return res


CFG_LOOP = ("python3 -c 'import os,time\n"
            "f=os.open(\"/sys/bus/pci/devices/0000:00:00.0/config\",os.O_RDONLY);n=100000\n"
            "t=time.perf_counter()\n"
            "for _ in range(n): os.pread(f,4,0)\n"
            "print((time.perf_counter()-t)/n*1e6)'")


IPI_LOOP = ("python3 -c 'import os,time\n"
            "a,b=os.pipe();c,d=os.pipe();n=20000\n"
            "if os.fork()==0:\n"
            " os.sched_setaffinity(0,{1})\n"
            " for _ in range(n): os.read(a,1);os.write(d,b\"x\")\n"
            " os._exit(0)\n"
            "os.sched_setaffinity(0,{0});time.sleep(0.2);t=time.perf_counter()\n"
            "for _ in range(n): os.write(b,b\"x\");os.read(c,1)\n"
            "print((time.perf_counter()-t)/n*1e6)'")


def run_ipi(vm):
    # Pipe ping-pong between processes pinned to vCPU 0 and 1: every hop
    # wakes a task on the other (idle) vCPU, so it measures how fast an
    # IPI gets a sleeping vCPU going.
    return round(min(float(vm.ssh(IPI_LOOP)) for _ in range(3)), 1)


def run_exit(vm):
    # Each 4-byte read of PCI config space is one ECAM access: one MMIO
    # exit, plus the guest's syscall overhead.
    return round(min(float(vm.ssh(CFG_LOOP)) for _ in range(3)), 2)


def one_run(a, i):
    run_dir = os.path.join(a.workdir, "runs", f"{a.tag}-{i}")
    shutil.rmtree(run_dir, ignore_errors=True)
    os.makedirs(run_dir)
    tests = set(a.tests.split(","))
    vm = Vm(a, run_dir)
    vm.start(with_data="fio" in tests)
    r = {"tag": a.tag, "vmm": a.vmm, "boot": a.boot, "bus": a.bus, "fmt": a.fmt, "run": i}
    try:
        r["boot_s"] = round(vm.wait_login(), 2)
        vm.wait_ssh()
        if "idle" in tests:
            # Let cloud-init and boot-time jobs finish.
            vm.ssh("cloud-init status --wait >/dev/null", check=False)
            time.sleep(10)
            c0, t0 = vm.cpu_seconds(), time.monotonic()
            time.sleep(20)
            r["idle_cpu_pct"] = round(100 * (vm.cpu_seconds() - c0) / (time.monotonic() - t0), 1)
            r["rss_mib"] = round(vm.rss_mib())
        if "exit" in tests:
            r["mmio_read_us"] = run_exit(vm)
            r["ipi_rtt_us"] = run_ipi(vm)
        if "fio" in tests:
            r["fio"] = run_fio(vm, a.fio_runtime, a.fio_only)
        if "net" in tests:
            r["net"] = run_net(vm, a.net_secs)
    except Exception as e:
        r["error"] = str(e)
    finally:
        vm.stop()
        # Disk images are big; the logs stay.
        for f in ("data.raw", "data.qcow2"):
            try:
                os.remove(os.path.join(run_dir, f))
            except FileNotFoundError:
                pass
    return r


def table(path):
    """Medians per tag of a results.jsonl, as a Markdown table: one column
    per tag (in the order they first appear), one row per number."""
    runs = {}
    for line in open(path):
        r = json.loads(line)
        if "error" in r:
            continue
        flat = {k: v for k, v in r.items() if isinstance(v, (int, float)) and k != "run"}
        for k, v in r.get("fio", {}).items():
            flat[f"fio {k} " + ("MB/s" if "1m" in k else "IOPS")] = v["mbs"] if "1m" in k else v["iops"]
            if "4k-qd1" in k:
                flat[f"fio {k} latency us"] = v["lat_us"]
        for k, v in (r.get("net") or {}).items():
            if v is not None:
                flat[f"net {k}"] = v
        runs.setdefault(r["tag"], []).append(flat)
    tags = list(runs)
    keys = []
    for t in tags:
        for f in runs[t]:
            keys += [k for k in f if k not in keys]
    def med(t, k):
        v = sorted(f[k] for f in runs[t] if k in f)
        if not v:
            return ""
        m = v[len(v) // 2] if len(v) % 2 else (v[len(v) // 2 - 1] + v[len(v) // 2]) / 2
        return f"{m:.3g}" if m < 100 else f"{m:.0f}"
    print("| | " + " | ".join(f"{t} (n={len(runs[t])})" for t in tags) + " |")
    print("|---" * (len(tags) + 1) + "|")
    for k in keys:
        print(f"| {k} | " + " | ".join(med(t, k) for t in tags) + " |")


def main():
    if len(sys.argv) == 3 and sys.argv[1] == "--table":
        return table(sys.argv[2])
    ap = argparse.ArgumentParser()
    ap.add_argument("workdir")
    ap.add_argument("--vmm", default="vroom", choices=["vroom", "qemu"])
    ap.add_argument("--bin")
    ap.add_argument("--boot", default="kernel", choices=["kernel", "uefi"])
    ap.add_argument("--bus", default="virtio", choices=["virtio", "nvme"])
    ap.add_argument("--fmt", default="raw", choices=["raw", "qcow2"])
    ap.add_argument("--tests", default="boot,idle,exit,fio,net")
    ap.add_argument("--runs", type=int, default=1)
    ap.add_argument("--tag")
    ap.add_argument("--fio-runtime", type=int, default=8)
    ap.add_argument("--net-secs", type=int, default=8)
    ap.add_argument("--fio-only", help="regex: run only the fio jobs it matches")
    a = ap.parse_args()
    a.workdir = os.path.abspath(a.workdir)
    a.bin = a.bin or (QEMU if a.vmm == "qemu" else os.path.join(ROOT, "target/release/vroom"))
    a.tag = a.tag or f"{a.vmm}-{a.boot}-{a.bus}-{a.fmt}"
    for i in range(a.runs):
        r = one_run(a, i)
        with open(os.path.join(a.workdir, "results.jsonl"), "a") as f:
            f.write(json.dumps(r) + "\n")
        print(json.dumps(r), flush=True)


if __name__ == "__main__":
    main()

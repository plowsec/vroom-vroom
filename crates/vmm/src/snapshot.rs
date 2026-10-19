//! Save / restore a whole VM. The VM is paused by the caller.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use vroom_hv::{IrqChipState, VcpuState};
use vroom_memory::GuestAddress;
use vroom_snapshot::{MemoryRange, SnapshotDir};

use crate::vcpu::VcpuCmd;
use crate::{Error, Result, Vmm, PAUSE_TIMEOUT};

const MEMORY: &str = "memory.bin";
const MEMORY_MAP: &str = "memory.map";
/// RAM is stored in chunks; all-zero chunks are skipped (bit clear in map).
const CHUNK: u64 = 64 << 10;
const IRQCHIP: &str = "irqchip.bin";

fn vcpu_blob(i: usize) -> String {
    format!("vcpu{i}.json")
}

fn dev_blob(id: &str) -> String {
    format!("dev.{id}")
}

/// Snapshots layer the disks; suspend images pin them instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Snapshot,
    Suspend,
}

/// What to do with the disks when starting from `dir`: for a snapshot,
/// the frozen layers to revert to; for a suspend image, nothing (but the
/// disks must not have changed since).
pub(crate) fn disk_plan(dir: &Path, state_dir: &Path) -> Result<BTreeMap<usize, PathBuf>> {
    let vm = crate::disks::vm_dir(state_dir);
    let snap = SnapshotDir::open(dir)?;
    let m = &snap.manifest.meta;
    let mut layers = BTreeMap::new();
    let suspend = m.get("kind").map(String::as_str) == Some("suspend");
    // The guest's RAM in the save caches what these files held then.
    for (k, v) in m.iter().filter(|(k, _)| k.starts_with("chain.")) {
        for line in v.lines() {
            let (stamp, path) = line.split_once('|').ok_or_else(|| Error::Setup(format!("bad {k}")))?;
            let path = crate::disks::found(path, &vm);
            let now = crate::disks::file_stamp(&path).unwrap_or_default();
            let path = path.display();
            if now != stamp {
                return Err(Error::Setup(if suspend {
                    format!("disk {path} changed since this VM was suspended; resuming would corrupt it (use --fresh)")
                } else {
                    format!("disk {path} changed since this snapshot was taken; restoring it would corrupt the guest's disk")
                }));
            }
        }
    }
    if suspend {
        return Ok(layers);
    }
    for (k, v) in m.iter() {
        if let Some(i) = k.strip_prefix("disk.") {
            let i: usize = i.parse().map_err(|_| Error::Setup(format!("bad {k}")))?;
            layers.insert(i, crate::disks::found(v, &vm));
        }
    }
    Ok(layers)
}

/// Saved network adapters have a control queue (saves from before it
/// don't say so, and have none).
const NET_CTRL: &str = "net.ctrl_vq";
/// How many removable shares for USB disks' files it has (saves from
/// before them have none).
const USB_FILES: &str = "usb.file_shares";
/// Network adapters in hot plug slots: "adapter:slot,..." (0-based).
const NET_HOT: &str = "net.hotplug";
/// Windows' 3D GPU has memory in BAR 0, the registers after the host
/// window (saves from before have the registers in BAR 0).
const GPU3D_VRAM: &str = "gpu.bar0_vram";

/// The network adapters the VM saved in `dir` had in hot plug slots:
/// adapter index to slot.
pub(crate) fn saved_hot_nets(dir: &Path) -> Result<BTreeMap<usize, usize>> {
    let snap = SnapshotDir::open(dir)?;
    let mut m = BTreeMap::new();
    let Some(v) = snap.manifest.meta.get(NET_HOT) else { return Ok(m) };
    for pair in v.split(',').filter(|p| !p.is_empty()) {
        let bad = || Error::Setup(format!("bad {NET_HOT}: {v}"));
        let (a, b) = pair.split_once(':').ok_or_else(bad)?;
        let (a, b) = (a.parse::<usize>().map_err(|_| bad())?, b.parse::<usize>().map_err(|_| bad())?);
        if b >= crate::hotplug::PORTS || m.values().any(|&x| x == b) || m.insert(a, b).is_some() {
            return Err(bad());
        }
    }
    Ok(m)
}

/// Whether the VM saved in `dir` has memory in its 3D GPU's BAR 0.
pub(crate) fn saved_gpu3d_vram(dir: &Path) -> bool {
    SnapshotDir::open(dir).is_ok_and(|s| s.manifest.meta.get(GPU3D_VRAM).is_some_and(|v| v == "1"))
}

/// The removable shares the VM saved in `dir` has.
pub(crate) fn saved_usb_file_slots(dir: &Path) -> usize {
    let Ok(snap) = SnapshotDir::open(dir) else { return 0 };
    snap.manifest.meta.get(USB_FILES).and_then(|n| n.parse::<usize>().ok()).unwrap_or(0).min(crate::usb::FILE_SLOTS)
}

/// The VM file as it was when `dir` was saved, and whether its network
/// adapters had a control queue.
pub(crate) fn saved_config(dir: &Path, state_dir: &Path) -> Result<(vroom_config::VmConfig, bool)> {
    let snap = SnapshotDir::open(dir)?;
    let mut c: vroom_config::VmConfig =
        serde_json::from_value(snap.manifest.config.clone()).map_err(|e| Error::Setup(format!("snapshot config: {e}")))?;
    c.rebase_paths(&crate::disks::vm_dir(state_dir));
    Ok((c, snap.manifest.meta.contains_key(NET_CTRL)))
}

pub(crate) fn save(vmm: &mut Vmm, dir: &Path, description: &str, kind: Kind) -> Result<()> {
    let vm = crate::disks::vm_dir(vmm.disks.state_dir());
    let mut snap = SnapshotDir::create(dir)?;
    match kind {
        Kind::Snapshot => {
            // Disk state at this instant becomes read-only; the VM goes on
            // writing to a fresh overlay.
            for (i, layer) in vmm.disks.freeze()? {
                snap.manifest.meta.insert(format!("disk.{i}"), crate::disks::recorded(&layer, &vm));
            }
        }
        Kind::Suspend => {
            snap.manifest.meta.insert("kind".into(), "suspend".into());
        }
    }
    for (i, files) in vmm.disks.chain_stamps(kind == Kind::Suspend)? {
        let lines: Vec<String> = files.iter().map(|(p, stamp)| format!("{stamp}|{}", crate::disks::recorded(p, &vm))).collect();
        snap.manifest.meta.insert(format!("chain.{i}"), lines.join("\n"));
    }
    // The network adapters as they are now, held still until the devices
    // are saved: none comes or goes meanwhile. One still on its way out
    // goes now (a snapshot holds none half gone).
    let nics = vmm.net_ports.clone();
    let nics = nics.hold();
    if let Some(h) = &vmm.hotplug {
        h.settle();
    }
    let (nets, hot) = crate::net::saved(&nics);
    vmm.config.nets = nets;
    let m = &mut snap.manifest;
    m.description = description.to_string();
    m.arch = "aarch64".into();
    m.backend = vmm.hv.name().into();
    let mut config = vmm.config.clone();
    config.relative_paths(&vm);
    m.config = serde_json::to_value(&config).map_err(|e| Error::Setup(e.to_string()))?;
    if !hot.is_empty() {
        let v: Vec<String> = hot.iter().map(|(a, b)| format!("{a}:{b}")).collect();
        m.meta.insert(NET_HOT.into(), v.join(","));
    }
    m.meta.insert("vcpus".into(), vmm.vcpus.len().to_string());
    if vmm.net_ctrl {
        m.meta.insert(NET_CTRL.into(), "1".into());
    }
    if vmm.usb_file_slots > 0 {
        m.meta.insert(USB_FILES.into(), vmm.usb_file_slots.to_string());
    }
    if vmm.gpu3d_vram {
        m.meta.insert(GPU3D_VRAM.into(), "1".into());
    }

    for (i, v) in vmm.vcpus.iter().enumerate() {
        let (tx, rx) = mpsc::channel();
        v.peer.send(VcpuCmd::Save(tx))?;
        let st = rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)??;
        let json = serde_json::to_vec(&st).map_err(|e| Error::Setup(e.to_string()))?;
        snap.put_blob(&vcpu_blob(i), &json)?;
    }

    snap.manifest.meta.insert("clock".into(), vmm.vm.clock().to_string());
    let gic = vmm.vm.save_irqchip()?;
    snap.put_blob(IRQCHIP, &gic.blob)?;
    snap.manifest.meta.insert("irqchip_backend".into(), gic.backend);

    for d in &vmm.devices {
        let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
        let id = d.snapshot_id();
        let data = d.save()?;
        snap.put_blob(&dev_blob(&id), &data)?;
    }

    let mut w = snap.stream_writer(MEMORY)?;
    let mut ranges = Vec::new();
    let mut map = Vec::new();
    let mut chunk_no = 0u64;
    let mut buf = vec![0u8; CHUNK as usize];
    for r in vmm.mem.regions() {
        ranges.push(MemoryRange { gpa: r.start().raw(), size: r.len(), offset: chunk_no * CHUNK });
        let mut off = 0;
        while off < r.len() {
            let n = CHUNK.min(r.len() - off) as usize;
            vmm.mem.read(GuestAddress(r.start().raw() + off), &mut buf[..n])?;
            if map.len() <= (chunk_no / 8) as usize {
                map.push(0u8);
            }
            if buf[..n].iter().any(|&b| b != 0) {
                map[(chunk_no / 8) as usize] |= 1 << (chunk_no % 8);
                w.write_all(&buf[..n])?;
            }
            off += n as u64;
            chunk_no += 1;
        }
    }
    snap.finish_stream(MEMORY, w)?;
    snap.put_blob(MEMORY_MAP, &map)?;
    snap.manifest.memory = ranges;
    snap.commit()?;
    log::info!("snapshot saved to {}", dir.display());
    Ok(())
}

pub(crate) fn restore(vmm: &mut Vmm, dir: &Path) -> Result<()> {
    let snap = SnapshotDir::open(dir)?;
    let m = &snap.manifest;
    if m.backend != vmm.hv.name() {
        return Err(Error::Setup(format!("snapshot is for backend {}, host is {}", m.backend, vmm.hv.name())));
    }
    let saved: vroom_config::VmConfig =
        serde_json::from_value(m.config.clone()).map_err(|e| Error::Setup(format!("snapshot config: {e}")))?;
    if saved.cpus != vmm.config.cpus || saved.memory_mib != vmm.config.memory_mib {
        return Err(Error::Setup("snapshot was taken with a different cpu/memory config".into()));
    }

    // RAM: ranges must match our regions exactly.
    let ours: Vec<(u64, u64)> = vmm.mem.regions().map(|r| (r.start().raw(), r.len())).collect();
    let theirs: Vec<(u64, u64)> = m.memory.iter().map(|r| (r.gpa, r.size)).collect();
    if ours != theirs {
        return Err(Error::Setup("snapshot memory layout mismatch".into()));
    }
    let map = snap.get_blob(MEMORY_MAP)?;
    let mut rd = snap.verified_reader(MEMORY)?;
    let mut chunk_no = 0u64;
    let mut buf = vec![0u8; CHUNK as usize];
    for r in &m.memory {
        if r.offset != chunk_no * CHUNK {
            return Err(Error::Setup("snapshot memory offsets not contiguous".into()));
        }
        let mut off = 0;
        while off < r.size {
            let n = CHUNK.min(r.size - off) as usize;
            let byte = map
                .get((chunk_no / 8) as usize)
                .ok_or_else(|| Error::Setup("memory map too short".into()))?;
            let addr = GuestAddress(r.gpa + off);
            if byte & (1 << (chunk_no % 8)) != 0 {
                rd.read_exact(&mut buf[..n])?;
                vmm.mem.write(addr, &buf[..n])?;
            } else {
                // Fresh VM RAM is zero, but be explicit (restore into a used VM).
                vmm.mem.fill(addr, n as u64, 0)?;
            }
            off += n as u64;
            chunk_no += 1;
        }
    }
    if rd.read(&mut [0u8; 1])? != 0 {
        return Err(Error::Setup("trailing data in memory.bin".into()));
    }

    let gic = IrqChipState {
        backend: m.meta.get("irqchip_backend").cloned().unwrap_or_default(),
        blob: snap.get_blob(IRQCHIP)?,
    };
    vmm.vm.restore_irqchip(&gic)?;
    let clock: u64 = m
        .meta
        .get("clock")
        .and_then(|c| c.parse().ok())
        .ok_or_else(|| Error::Setup("snapshot has no clock".into()))?;
    // Before vCPU restore: each vCPU picks the VM clock up as it restores.
    vmm.vm.set_clock(clock);

    // Devices before vCPUs: a vCPU runs as soon as it is restored, and the
    // guest must not see a device half back (the GPU's shared memory window
    // stays empty until its buffers are made again: a Vulkan driver reading
    // its ring there gave up).
    for d in &vmm.devices {
        let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
        let id = d.snapshot_id();
        d.restore(&snap.get_blob(&dev_blob(&id))?)?;
    }

    let mut cpus = vmm.shared.cpus.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for (i, v) in vmm.vcpus.iter().enumerate() {
        let st: VcpuState = serde_json::from_slice(&snap.get_blob(&vcpu_blob(i))?)
            .map_err(|e| Error::Setup(format!("vcpu{i} state: {e}")))?;
        if st.index as usize != i {
            return Err(Error::Setup(format!("vcpu{i} state has index {}", st.index)));
        }
        cpus.set_on(i, st.powered_on);
        let (tx, rx) = mpsc::channel();
        let on = st.powered_on;
        v.peer.send(VcpuCmd::Restore(Box::new(st), on, tx))?;
        rx.recv_timeout(PAUSE_TIMEOUT).map_err(|_| Error::Timeout)??;
    }
    *vmm.shared.cpus.lock().unwrap_or_else(|e| e.into_inner()) = cpus;

    log::info!("restored snapshot from {}", dir.display());
    Ok(())
}

//! Save / restore a whole VM. The VM is paused by the caller.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;

use bulwark_hv::{IrqChipState, VcpuState};
use bulwark_memory::GuestAddress;
use bulwark_snapshot::{MemoryRange, SnapshotDir};

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

pub(crate) fn save(vmm: &mut Vmm, dir: &Path, description: &str) -> Result<()> {
    let mut snap = SnapshotDir::create(dir)?;
    let m = &mut snap.manifest;
    m.description = description.to_string();
    m.arch = "aarch64".into();
    m.backend = vmm.hv.name().into();
    m.config = serde_json::to_value(&vmm.config).map_err(|e| Error::Setup(e.to_string()))?;
    m.meta.insert("vcpus".into(), vmm.vcpus.len().to_string());

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
    let saved: bulwark_config::VmConfig =
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

    for d in &vmm.devices {
        let mut d = d.lock().unwrap_or_else(|e| e.into_inner());
        let id = d.snapshot_id();
        d.restore(&snap.get_blob(&dev_blob(&id))?)?;
    }
    log::info!("restored snapshot from {}", dir.display());
    Ok(())
}

//! vGICv3 state save/restore through KVM device attributes.
//!
//! Same register set and ordering as QEMU (arm_gicv3_kvm.c): distributor
//! control, redistributor SGI/PPI frames, distributor SPI state, line
//! levels, then the CPU interface (ICC_*) per vCPU. No ITS / LPIs: the board
//! uses a GICv2m-style frame for MSIs.
//!
//! The blob is a flat list of 20-byte records `(group u32, attr u64,
//! value u64)`, little endian, in restore order. Restore input comes from
//! disk, so groups are checked against the four we write and the kernel
//! validates attrs and values.

use kvm_bindings::{
    kvm_device_attr, KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS, KVM_DEV_ARM_VGIC_GRP_DIST_REGS, KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO,
    KVM_DEV_ARM_VGIC_GRP_REDIST_REGS, KVM_DEV_ARM_VGIC_LINE_LEVEL_INFO_SHIFT, KVM_DEV_ARM_VGIC_V3_MPIDR_SHIFT,
    VGIC_LEVEL_INFO_LINE_LEVEL,
};
use kvm_ioctls::DeviceFd;

use super::kerr;
use crate::aarch64::mpidr_for;
use crate::{Error, Result};

const REC: usize = 20;
/// Way more than 992 SPIs and 512 vCPUs need; caps a hostile blob.
const MAX_RECORDS: usize = 1 << 16;

// Distributor offsets.
const GICD_CTLR: u64 = 0x0000;
const GICD_IGROUPR: u64 = 0x0080;
const GICD_ISENABLER: u64 = 0x0100;
const GICD_ICENABLER: u64 = 0x0180;
const GICD_ISPENDR: u64 = 0x0200;
const GICD_ICPENDR: u64 = 0x0280;
const GICD_ISACTIVER: u64 = 0x0300;
const GICD_ICACTIVER: u64 = 0x0380;
const GICD_IPRIORITYR: u64 = 0x0400;
const GICD_ICFGR: u64 = 0x0c00;
const GICD_IROUTER: u64 = 0x6000;

// Redistributor SGI frame (second 64K page).
const SGI: u64 = 0x1_0000;
const GICR_IGROUPR0: u64 = SGI + 0x0080;
const GICR_ISENABLER0: u64 = SGI + 0x0100;
const GICR_ISPENDR0: u64 = SGI + 0x0200;
const GICR_ISACTIVER0: u64 = SGI + 0x0300;
const GICR_IPRIORITYR: u64 = SGI + 0x0400;
const GICR_ICFGR1: u64 = SGI + 0x0c04;
/// ISxxxR -> ICxxxR is +0x80 in both frames.
const CLEAR_DELTA: u64 = 0x80;

/// ICC_* sysreg ids as KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS wants them
/// (op0:op1:crn:crm:op2 packed like KVM_REG_ARM64_SYSREG).
const fn icc(op0: u64, op1: u64, crn: u64, crm: u64, op2: u64) -> u64 {
    op0 << 14 | op1 << 11 | crn << 7 | crm << 3 | op2
}
const ICC_SRE_EL1: u64 = icc(3, 0, 12, 12, 5);
const ICC_CTLR_EL1: u64 = icc(3, 0, 12, 12, 4);
const ICC_IGRPEN0_EL1: u64 = icc(3, 0, 12, 12, 6);
const ICC_IGRPEN1_EL1: u64 = icc(3, 0, 12, 12, 7);
const ICC_PMR_EL1: u64 = icc(3, 0, 4, 6, 0);
const ICC_BPR0_EL1: u64 = icc(3, 0, 12, 8, 3);
const ICC_BPR1_EL1: u64 = icc(3, 0, 12, 12, 3);
const fn icc_ap0r(n: u64) -> u64 {
    icc(3, 0, 12, 8, 4 + n)
}
const fn icc_ap1r(n: u64) -> u64 {
    icc(3, 0, 12, 9, n)
}

/// Affinity as the vGIC attr encodes it (Aff3:Aff2:Aff1:Aff0, 32 bits).
fn attr_mpidr(index: u32) -> u64 {
    let m = mpidr_for(index);
    let packed = ((m >> 32) & 0xff) << 24 | (m & 0xff_ffff);
    packed << KVM_DEV_ARM_VGIC_V3_MPIDR_SHIFT
}

fn access(dev: &DeviceFd, group: u32, attr: u64, val: &mut u64, write: bool) -> Result<()> {
    // CPU sysregs are 64-bit, everything else 32-bit.
    let mut v32 = *val as u32;
    let addr = if group == KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS {
        val as *mut u64 as u64
    } else {
        &mut v32 as *mut u32 as u64
    };
    let mut a = kvm_device_attr { group, attr, addr, flags: 0 };
    if write {
        dev.set_device_attr(&a).map_err(kerr("vgic restore"))?;
    } else {
        // SAFETY: addr points at a live u32/u64 matching the group's size.
        unsafe { dev.get_device_attr(&mut a) }.map_err(kerr("vgic save"))?;
        if group != KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS {
            *val = v32 as u64;
        }
    }
    Ok(())
}

/// Every (group, attr) to save, in restore order.
fn plan(vcpus: u32, nr_irqs: u32) -> Vec<(u32, u64)> {
    use KVM_DEV_ARM_VGIC_GRP_DIST_REGS as D;
    use KVM_DEV_ARM_VGIC_GRP_REDIST_REGS as R;
    let mut p = vec![(D, GICD_CTLR)];
    for c in 0..vcpus {
        let m = attr_mpidr(c);
        p.push((R, m | GICR_IGROUPR0));
        p.push((R, m | GICR_ICFGR1));
        p.extend((0..8).map(|i| (R, m | (GICR_IPRIORITYR + 4 * i))));
        p.extend([GICR_ISENABLER0, GICR_ISPENDR0, GICR_ISACTIVER0].map(|o| (R, m | o)));
    }
    let spis = || (32..nr_irqs).step_by(32).map(|n| n as u64);
    p.extend(spis().map(|n| (D, GICD_IGROUPR + n / 8)));
    for n in 32..nr_irqs as u64 {
        p.push((D, GICD_IROUTER + 8 * n));
        p.push((D, GICD_IROUTER + 8 * n + 4));
    }
    p.extend((32..nr_irqs as u64).step_by(16).map(|n| (D, GICD_ICFGR + n / 4)));
    p.extend((32..nr_irqs as u64).step_by(4).map(|n| (D, GICD_IPRIORITYR + n)));
    let line = (VGIC_LEVEL_INFO_LINE_LEVEL as u64) << KVM_DEV_ARM_VGIC_LINE_LEVEL_INFO_SHIFT;
    p.extend(spis().map(|n| (KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO, line | n)));
    for base in [GICD_ISPENDR, GICD_ISENABLER, GICD_ISACTIVER] {
        p.extend(spis().map(|n| (D, base + n / 8)));
    }
    p
}

fn cpu_plan(vcpus: u32) -> Vec<(u32, u64)> {
    let mut p = Vec::new();
    for c in 0..vcpus {
        let m = attr_mpidr(c);
        for r in [ICC_SRE_EL1, ICC_CTLR_EL1, ICC_IGRPEN0_EL1, ICC_IGRPEN1_EL1, ICC_PMR_EL1, ICC_BPR0_EL1, ICC_BPR1_EL1] {
            p.push((KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS, m | r));
        }
        for n in 0..4 {
            p.push((KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS, m | icc_ap0r(n)));
            p.push((KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS, m | icc_ap1r(n)));
        }
    }
    p
}

pub(super) fn save(dev: &DeviceFd, vcpus: u32, nr_irqs: u32) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut put = |g: u32, a: u64, v: u64| {
        out.extend_from_slice(&g.to_le_bytes());
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(&v.to_le_bytes());
    };
    for (g, a) in plan(vcpus, nr_irqs) {
        let mut v = 0;
        access(dev, g, a, &mut v, false)?;
        put(g, a, v);
    }
    for (g, a) in cpu_plan(vcpus) {
        let mut v = 0;
        // APnR beyond what the CPU interface implements don't exist.
        if access(dev, g, a, &mut v, false).is_ok() {
            put(g, a, v);
        }
    }
    Ok(out)
}

/// Set-type registers are restored as "clear all, then set".
fn clear_first(group: u32, attr: u64) -> Option<u64> {
    let off = attr & 0xffff_ffff;
    let set_regs = match group {
        KVM_DEV_ARM_VGIC_GRP_DIST_REGS => {
            (GICD_ISENABLER..GICD_ICENABLER).contains(&off)
                || (GICD_ISPENDR..GICD_ICPENDR).contains(&off)
                || (GICD_ISACTIVER..GICD_ICACTIVER).contains(&off)
        }
        KVM_DEV_ARM_VGIC_GRP_REDIST_REGS => [GICR_ISENABLER0, GICR_ISPENDR0, GICR_ISACTIVER0].contains(&off),
        _ => false,
    };
    set_regs.then_some(attr + CLEAR_DELTA)
}

pub(super) fn restore(dev: &DeviceFd, blob: &[u8]) -> Result<()> {
    if blob.len() % REC != 0 || blob.len() / REC > MAX_RECORDS {
        return Err(Error::Invalid("vgic state: bad size".into()));
    }
    for r in blob.chunks_exact(REC) {
        let g = u32::from_le_bytes(r[0..4].try_into().unwrap_or_default());
        let a = u64::from_le_bytes(r[4..12].try_into().unwrap_or_default());
        let mut v = u64::from_le_bytes(r[12..20].try_into().unwrap_or_default());
        if ![
            KVM_DEV_ARM_VGIC_GRP_DIST_REGS,
            KVM_DEV_ARM_VGIC_GRP_REDIST_REGS,
            KVM_DEV_ARM_VGIC_GRP_CPU_SYSREGS,
            KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO,
        ]
        .contains(&g)
        {
            return Err(Error::Invalid(format!("vgic state: group {g}")));
        }
        if let Some(clear) = clear_first(g, a) {
            access(dev, g, clear, &mut 0xffff_ffff, true)?;
        }
        access(dev, g, a, &mut v, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpidr_attr_packing() {
        assert_eq!(attr_mpidr(0), 0);
        assert_eq!(attr_mpidr(17), 0x101 << 32);
    }

    #[test]
    fn plan_is_sane() {
        let p = plan(2, 992);
        assert_eq!(p[0], (KVM_DEV_ARM_VGIC_GRP_DIST_REGS, GICD_CTLR));
        // 30 SPI words each for group, line level, pend, enable, active.
        assert_eq!(p.iter().filter(|(g, _)| *g == KVM_DEV_ARM_VGIC_GRP_LEVEL_INFO).count(), 30);
        assert_eq!(clear_first(KVM_DEV_ARM_VGIC_GRP_DIST_REGS, GICD_ISENABLER + 4), Some(GICD_ICENABLER + 4));
        assert_eq!(clear_first(KVM_DEV_ARM_VGIC_GRP_DIST_REGS, GICD_IPRIORITYR), None);
        assert_eq!(clear_first(KVM_DEV_ARM_VGIC_GRP_REDIST_REGS, 1 << 32 | GICR_ISPENDR0), Some(1 << 32 | (SGI + 0x280)));
    }
}

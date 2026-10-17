//! Devices plugged in and out while the VM runs: virtio devices in the
//! PCI Express root ports' slots (see `vroom_devices::pci::port`).
//!
//! A VM on PCI (UEFI boot, or `virtio_transport = "pci"`) gets [`PORTS`]
//! empty root ports after its other devices, so those keep their slots.
//! Network adapters use them today; the mechanism takes any virtio
//! device (a disk would work the same way).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use vroom_devices::pci::{IntxRouter, PciRoot, SharedPciDevice};
use vroom_memory::GuestMemory;
use vroom_virtio::pci::VirtioPci;
use vroom_virtio::VirtioDevice;

use crate::irq::GicMsi;

/// Hot plug slots a VM on PCI has.
pub(crate) const PORTS: usize = 4;
/// Devices in port N are named as if in root bus slot 0x80 + N (their
/// snapshot id: "virtio-net@80"), apart from the root bus's.
const ID_BASE: u8 = 0x80;
/// How long the guest has to let go of a device we asked back, before
/// it is pulled out anyway.
const GRACE: Duration = Duration::from_secs(15);

pub(crate) struct Hotplug {
    root: Arc<PciRoot>,
    mem: GuestMemory,
    msi: Arc<GicMsi>,
    intx: Arc<IntxRouter>,
    /// Per port: removals asked for (a late timer leaves a newer one be).
    asked: Vec<AtomicU64>,
}

impl std::fmt::Debug for Hotplug {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Hotplug").field("ports", &self.root.ports()).finish()
    }
}

impl Hotplug {
    pub fn new(root: Arc<PciRoot>, mem: GuestMemory, msi: Arc<GicMsi>, intx: Arc<IntxRouter>) -> Arc<Hotplug> {
        let asked = (0..root.ports()).map(|_| AtomicU64::new(0)).collect();
        Arc::new(Hotplug { root, mem, msi, intx, asked })
    }

    /// `dev` as the PCI function for port `port`: PCI Express, its INTx
    /// through the port's slot.
    pub fn function(&self, port: usize, dev: Box<dyn VirtioDevice>) -> SharedPciDevice {
        port_function(&self.root, port, dev, &self.mem, &self.msi, &self.intx)
    }

    /// Plug `dev` into a free port; the guest brings it up. Its port.
    pub fn add(&self, dev: Box<dyn VirtioDevice>) -> Result<usize, String> {
        let port = self.root.free_port().ok_or("no free hot plug slot (one taken out may still be on its way out)")?;
        let f = self.function(port, dev);
        self.root.plug(port, f)?;
        log::info!("hotplug: device in slot {}", port + 1);
        Ok(port)
    }

    /// Ask the guest to give back port `port`'s device; it is pulled out
    /// anyway if the guest hasn't let go after a while.
    pub fn remove(self: &Arc<Self>, port: usize) {
        let Some(asked) = self.asked.get(port) else { return };
        let n = asked.fetch_add(1, Ordering::SeqCst) + 1;
        if !self.root.unplug(port) {
            return;
        }
        if !self.root.removing(port) {
            log::info!("hotplug: slot {} emptied (the guest has no hot plug driver running)", port + 1);
            return;
        }
        let me: Weak<Hotplug> = Arc::downgrade(self);
        let r = std::thread::Builder::new().name("hotplug".into()).spawn(move || {
            std::thread::sleep(GRACE);
            let Some(h) = me.upgrade() else { return };
            if h.asked[port].load(Ordering::SeqCst) == n && h.root.removing(port) {
                log::warn!("hotplug: the guest didn't let go of slot {}'s device in {}s: pulled out", port + 1, GRACE.as_secs());
                h.root.force_unplug(port);
            }
        });
        if let Err(e) = r {
            log::warn!("hotplug: no timer for slot {} ({e}): pulled out now", port + 1);
            self.root.force_unplug(port);
        }
    }

    /// Finish removals still waiting for the guest (before a save: a
    /// snapshot holds no device on its way out).
    pub fn settle(&self) {
        for port in 0..self.root.ports() {
            if self.root.removing(port) {
                log::info!("hotplug: slot {}'s device pulled out for the save", port + 1);
                self.root.force_unplug(port);
            }
        }
    }
}

/// See [`Hotplug::function`]; also for a resumed VM's devices, before the
/// root complex exists (`slot`: the port's root bus slot).
pub(crate) fn function_in(slot: u8, port: usize, dev: Box<dyn VirtioDevice>, mem: &GuestMemory, msi: &Arc<GicMsi>, intx: &Arc<IntxRouter>) -> SharedPciDevice {
    // Device 0 behind the port, INTA: the port's own INTA (the standard
    // swizzle through a bridge).
    let v = VirtioPci::new(ID_BASE + port as u8, dev, mem.clone(), intx.pin(slot, 1), msi.clone()).with_pcie();
    Arc::new(Mutex::new(v))
}

fn port_function(root: &PciRoot, port: usize, dev: Box<dyn VirtioDevice>, mem: &GuestMemory, msi: &Arc<GicMsi>, intx: &Arc<IntxRouter>) -> SharedPciDevice {
    let slot = root.port_slot(port).unwrap_or(0);
    function_in(slot, port, dev, mem, msi, intx)
}

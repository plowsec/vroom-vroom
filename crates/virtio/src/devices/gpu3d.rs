//! What virtio-gpu needs from a host 3D renderer (virglrenderer / Venus).
//!
//! Kept as a trait so this crate stays free of unsafe code: the FFI lives
//! in `vroom-gpu3d`. Every call comes from the GPU worker thread.

use std::os::fd::RawFd;

/// Host copy of guest RAM ranges handed to the renderer. Built from
/// [`vroom_memory::GuestMemory::host_ranges`], so always inside guest RAM.
pub type HostIov = Vec<(*mut u8, usize)>;

/// virtio_gpu_resource_create_3d, minus the id.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Create3d {
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Box3d {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transfer3d {
    pub resource: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
    pub bx: Box3d,
    pub offset: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Blob {
    pub resource: u32,
    pub ctx: u32,
    pub mem: u32,
    pub flags: u32,
    pub id: u64,
    pub size: u64,
}

/// A host buffer the guest may map through the shared memory window.
#[derive(Debug, Clone, Copy)]
pub struct Mapping {
    pub ptr: *mut u8,
    pub size: u64,
    /// VIRTIO_GPU_MAP_CACHE_*.
    pub map_info: u32,
}

/// A signaled fence. `ring` is None for global (ctx 0 style) fences.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceDone {
    pub ctx: u32,
    pub ring: Option<u8>,
    pub fence: u64,
}

pub trait Renderer3d: Send {
    /// `(capset id, max version, max size)`.
    fn capsets(&self) -> Vec<(u32, u32, u32)>;
    fn capset(&mut self, id: u32, version: u32) -> Option<Vec<u8>>;

    fn context_create(&mut self, ctx: u32, capset: u32, name: &[u8]) -> bool;
    fn context_destroy(&mut self, ctx: u32);
    fn context_attach(&mut self, ctx: u32, resource: u32);
    fn context_detach(&mut self, ctx: u32, resource: u32);
    fn submit(&mut self, ctx: u32, cmds: &mut [u8]) -> bool;

    fn create_3d(&mut self, resource: u32, args: &Create3d) -> bool;
    fn attach_backing(&mut self, resource: u32, iov: HostIov) -> bool;
    fn detach_backing(&mut self, resource: u32);
    fn transfer(&mut self, to_host: bool, ctx: u32, t: &Transfer3d) -> bool;
    fn create_blob(&mut self, blob: &Blob, iov: HostIov) -> bool;
    fn map_blob(&mut self, resource: u32) -> Option<Mapping>;
    fn unmap_blob(&mut self, resource: u32);
    /// Copy out of a mapped blob (scanout of host memory).
    fn read_mapped(&mut self, resource: u32, offset: u64, out: &mut [u8]) -> bool;
    /// Read a box of a GL resource (level 0) back from the GPU into `out`,
    /// rows `stride` bytes apart, top row first (scanout of GL frames).
    fn read_back(&mut self, resource: u32, bx: Box3d, stride: u32, out: &mut [u8]) -> bool;
    /// Like [`Renderer3d::read_back`], but as BGRX rows (`bx.w * 4` bytes
    /// each) by a faster path. False if the renderer can't: then use
    /// `read_back`.
    fn read_scanout(&mut self, _resource: u32, _bx: Box3d, _out: &mut [u8]) -> bool {
        false
    }
    fn unref(&mut self, resource: u32);

    /// Fence on `ctx`'s timeline `ring`, or a global fence (ring None).
    fn create_fence(&mut self, ctx: u32, ring: Option<u8>, fence: u64) -> bool;
    /// Readable when fences may have completed.
    fn poll_fd(&self) -> Option<RawFd>;
    /// Collect completed fences.
    fn poll(&mut self) -> Vec<FenceDone>;
    /// Drop every context and resource (device reset).
    fn reset(&mut self);
    /// The calling thread is done with the renderer (a GPU worker
    /// stopping): another thread may take it over.
    fn release_thread(&mut self) {}
}

/// Puts renderer buffers into the guest's host-visible window (the VMM
/// implements this with the hypervisor's memory mapping).
pub trait ShmMapper: Send + Sync {
    fn map(&self, gpa: u64, m: &Mapping) -> bool;
    fn unmap(&self, gpa: u64, size: u64);
}

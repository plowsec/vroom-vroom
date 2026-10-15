//! A VMware disk to import: sparse extent headers, grain tables,
//! compressed grains, descriptors, parent chains.
#![no_main]
use vroom_block::vmdk::VmdkImage;
use vroom_block::DiskImage;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&mode, data)) = data.split_first() else { return };
    let td = tempfile::tempdir().unwrap();
    let dir = td.path();
    let write = |name: &str, b: &[u8]| std::fs::write(dir.join(name), b).unwrap();
    match mode % 3 {
        // A monolithic sparse file, descriptor embedded.
        0 => write("disk.vmdk", data),
        // A well-formed descriptor; the extent and its parent are input bytes.
        1 => {
            if data.len() < 12 {
                return;
            }
            let (h, rest) = data.split_at(12);
            let types = ["monolithicSparse", "twoGbMaxExtentSparse", "streamOptimized", "monolithicFlat", "vmfs"];
            let kinds = ["SPARSE", "FLAT", "ZERO", "VMFS"];
            let sectors = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
            let offset = u16::from_le_bytes([h[4], h[5]]);
            let cid = u32::from_le_bytes([h[6], h[7], h[8], h[9]]);
            let parent = if h[10] & 1 != 0 { format!("parentCID={cid:08x}\nparentFileNameHint=\"p.vmdk\"\n") } else { String::new() };
            let ty = types[h[11] as usize % types.len()];
            let kind = kinds[(h[11] >> 3) as usize % kinds.len()];
            let ext = match kind {
                "ZERO" => format!("RW {sectors} ZERO"),
                "FLAT" => format!("RW {sectors} FLAT \"x.vmdk\" {offset}"),
                _ => format!("RW {sectors} {kind} \"x.vmdk\""),
            };
            let desc = format!("# Disk DescriptorFile\nversion=1\nCID=fffffffe\n{parent}createType=\"{ty}\"\n{ext}\n");
            write("disk.vmdk", desc.as_bytes());
            write("x.vmdk", rest);
            write("p.vmdk", rest);
        }
        // Any text as the descriptor, the rest as an extent.
        _ => {
            let Some((&len, data)) = data.split_first() else { return };
            let n = (len as usize * 4).min(data.len());
            write("disk.vmdk", &data[..n]);
            write("x.vmdk", &data[n..]);
        }
    }
    let Ok(img) = VmdkImage::open(&dir.join("disk.vmdk")) else { return };
    let size = img.size();
    let mut buf = vec![0u8; 4096];
    for off in [0, size / 3, size / 2 + 1, size.saturating_sub(4096)] {
        let n = 4096.min(size.saturating_sub(off)) as usize;
        let _ = img.read_at(&mut buf[..n], off);
    }
    let _ = img.files();
});

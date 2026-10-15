use std::path::Path;

use super::*;

/// A fake .vmwarevm bundle: the .vmx plus empty files for its disks.
fn bundle(dir: &Path, name: &str, vmx: &str, files: &[&str]) -> PathBuf {
    let b = dir.join(format!("{name}.vmwarevm"));
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(b.join(format!("{name}.vmx")), vmx).unwrap();
    for f in files {
        std::fs::write(b.join(f), b"").unwrap();
    }
    b
}

const WIN: &str = r#".encoding = "UTF-8"
displayName = "Windows 11 64-bit Arm 2"
firmware = "efi"
guestOS = "arm-windows11-64"
numvcpus = "4"
memsize = "12096"
nvme0.present = "TRUE"
nvme0:0.fileName = "Virtual Disk-000003.vmdk"
nvme0:0.present = "TRUE"
sata0.present = "TRUE"
sata0:1.deviceType = "cdrom-image"
sata0:1.fileName = "ISO_PATH"
sata0:1.present = "TRUE"
ethernet0.connectionType = "nat"
ethernet0.virtualDev = "vmxnet3"
ethernet0.present = "TRUE"
vtpm.present = "TRUE"
checkpoint.vmState = ""
encryption.keySafe = "vmware:key/list/..."
encryption.data = "..."
uuid.bios = "56 4d 01 02 03 04 05 06-07 08 09 0a 0b 0c 0d 0e"
"#;

#[test]
fn windows_11_from_fusion() {
    let t = tempfile::tempdir().unwrap();
    let iso = t.path().join("win.iso");
    std::fs::write(&iso, b"").unwrap();
    let b = bundle(t.path(), "Windows 11 64-bit Arm 2", &WIN.replace("ISO_PATH", &iso.display().to_string()), &["Virtual Disk-000003.vmdk"]);
    let p = plan(&b).unwrap();
    assert_eq!(p.name, "Windows-11-64-bit-Arm-2");
    assert_eq!(p.os, GuestOs::Windows);
    assert_eq!((p.cpus, p.memory_mib, p.nets), (4, 12096, 1));
    assert!(p.tpm && !p.suspended && !p.audio);
    assert_eq!(p.disks.len(), 2);
    assert_eq!((p.disks[0].bus, p.disks[0].cdrom), (DiskBus::Nvme, false));
    assert!(p.disks[1].cdrom && p.disks[1].source == iso);
    assert!(p.notes.iter().any(|n| n.contains("TPM")));
    assert!(p.notes.iter().any(|n| n.contains("snapshots")), "-000003 is a snapshot delta");
    // uuid.bios is raw SMBIOS bytes; vroom keeps the usual order.
    assert_eq!(p.uuid, parse_uuid_bios("56 4d 01 02 03 04 05 06-07 08 09 0a 0b 0c 0d 0e").map(smbios_to_rfc));

    // The VM file it writes is valid, and names its disks.
    let text = render(&p, &p.name, &["w.qcow2".into()], &Options::default(), [1, 2, 3, 4, 5, 6, 7, 8]);
    let c = VmConfig::from_toml(&text).unwrap();
    assert_eq!(c.disks.len(), 2);
    assert!(c.tpm && c.disks[1].cdrom);
    assert_eq!(c.identity.serial.as_deref(), Some("010203040506"));
    let keep = render(&p, &p.name, &["w.qcow2".into()], &Options { keep_hardware_ids: true, ..Default::default() }, [0; 8]);
    let c = VmConfig::from_toml(&keep).unwrap();
    assert_eq!(c.identity.vendor, "VMware, Inc.");
    assert!(c.identity.serial.unwrap().starts_with("VMware-56 4d"));
}

#[test]
fn uuid_byte_order() {
    let raw = parse_uuid_bios("56 4d 01 02 03 04 05 06-07 08 09 0a 0b 0c 0d 0e").unwrap();
    assert_eq!(raw[0], 0x56);
    let rfc = smbios_to_rfc(raw);
    assert_eq!(rfc[..8], [0x02, 0x01, 0x4d, 0x56, 0x04, 0x03, 0x06, 0x05]);
    assert_eq!(rfc[8..], raw[8..]);
    assert!(parse_uuid_bios("56 4d").is_none());
}

#[test]
fn suspended_ubuntu_with_extras() {
    let t = tempfile::tempdir().unwrap();
    let vmx = r#"displayName = "Ubuntu Work"
guestOS = "arm-ubuntu-64"
numvcpus = "4"
memsize = "10752"
nvme0:0.fileName = "Virtual Disk-cl1-000001.vmdk"
nvme0:0.present = "TRUE"
sata0:0.deviceType = "cdrom-raw"
sata0:0.fileName = "auto detect"
sata0:0.present = "TRUE"
sata0:1.deviceType = "cdrom-image"
sata0:1.fileName = "/nowhere/jammy.iso"
sata0:1.present = "TRUE"
sata0:2.fileName = "data.vmdk"
sata0:2.present = "TRUE"
sharedFolder0.present = "TRUE"
sharedFolder0.enabled = "TRUE"
sharedFolder0.writeAccess = "TRUE"
sharedFolder0.hostPath = "/Users/me/src"
sharedFolder0.guestName = "my code"
sharedFolder1.present = "TRUE"
sharedFolder1.hostPath = "relative/ignored"
sound.present = "TRUE"
checkpoint.vmState = "Ubuntu Work-cd413428.vmss"
ethernet0.connectionType = "nat"
ethernet0.present = "TRUE"
ethernet1.connectionType = "custom"
ethernet1.present = "TRUE"
"#;
    let b = bundle(t.path(), "Ubuntu Work", vmx, &[]);
    let p = plan(&b).unwrap();
    assert!(p.suspended && p.audio);
    assert_eq!(p.os, GuestOs::Ubuntu);
    // nvme stays nvme (fstab may name it); a SATA data disk goes virtio.
    assert_eq!(p.disks.iter().map(|d| (d.slot.as_str(), d.bus)).collect::<Vec<_>>(), [("nvme0:0", DiskBus::Nvme), ("sata0:2", DiskBus::Virtio)]);
    assert_eq!(p.nets, 2);
    assert_eq!(p.shares.len(), 1);
    assert_eq!((p.shares[0].tag.as_str(), p.shares[0].readonly), ("my-code", false));
    assert!(p.notes.iter().any(|n| n.contains("custom")));
    assert!(p.notes.iter().any(|n| n.contains("jammy.iso")));
    // Suspended: refused unless asked.
    let err = run(&p, t.path(), &Options::default(), |_| {}).unwrap_err();
    assert!(err.to_string().contains("suspended"));
    let text = render(&p, "work", &["a.qcow2".into(), "b.qcow2".into()], &Options::default(), [9; 8]);
    let c = VmConfig::from_toml(&text).unwrap();
    assert_eq!((c.nets.len(), c.shares.len(), c.audio.output), (2, 1, true));
}

#[test]
fn refusals() {
    let t = tempfile::tempdir().unwrap();
    let x86 = bundle(t.path(), "old", "guestOS = \"windows11-64\"\nnvme0:0.fileName = \"d.vmdk\"\nnvme0:0.present = \"TRUE\"\n", &[]);
    assert!(plan(&x86).unwrap_err().to_string().contains("x86"));

    let open = bundle(t.path(), "busy", "guestOS = \"arm-ubuntu-64\"\nnvme0:0.fileName = \"d.vmdk\"\nnvme0:0.present = \"TRUE\"\n", &[]);
    std::fs::create_dir(open.join("busy.vmx.lck")).unwrap();
    assert!(plan(&open).unwrap_err().to_string().contains("VMware has this VM open"));

    let locked = bundle(t.path(), "enc", "encryption.keySafe = \"x\"\nencryption.data = \"y\"\n", &[]);
    assert!(plan(&locked).unwrap_err().to_string().contains("encrypted"));

    let nodisk = bundle(t.path(), "empty", "guestOS = \"arm-debian12-64\"\n", &[]);
    assert!(plan(&nodisk).unwrap_err().to_string().contains("no disk"));

    let two = t.path().join("two.vmwarevm");
    std::fs::create_dir(&two).unwrap();
    std::fs::write(two.join("a.vmx"), "").unwrap();
    std::fs::write(two.join("b.vmx"), "").unwrap();
    assert!(plan(&two).unwrap_err().to_string().contains("several"));
}

#[test]
fn names() {
    assert_eq!(clean_name("Ubuntu 64-bit Arm 26.04"), "Ubuntu-64-bit-Arm-26.04");
    assert_eq!(clean_name("  ..Hidden / Name!! "), "Hidden-Name");
    assert!(is_delta_name("Virtual Disk-cl1-000001"));
    assert!(!is_delta_name("Virtual Disk-cl1"));
    assert!(slot_order("nvme0:0") < slot_order("sata0:0"));
    assert!(slot_order("sata0:2") < slot_order("sata1:0"));
}

#[test]
fn copy_skips_zeros() {
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("src.raw");
    let mut data = vec![0u8; 3 << 20];
    data[(2 << 20) + 5] = 7;
    std::fs::write(&src, &data).unwrap();
    let img = vroom_block::open(&src, Some(vroom_block::Format::Raw), true).unwrap();
    let dst = t.path().join("out.qcow2");
    let mut last = (0, 0);
    copy_disk(img.as_ref(), &dst, |d, n| last = (d, n)).unwrap();
    assert_eq!(last, (3 << 20, 3 << 20));
    let out = vroom_block::open(&dst, Some(vroom_block::Format::Qcow2), true).unwrap();
    let mut back = vec![0u8; 3 << 20];
    out.read_at(&mut back, 0).unwrap();
    assert_eq!(back, data);
    // Only the one non-zero MiB was written.
    assert!(std::fs::metadata(&dst).unwrap().len() < 2 << 20);
}

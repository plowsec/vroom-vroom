//! An update for a running guest agent: one system's tools folder as
//! agent frames, `UpdateBegin` (the manifest: paths, sizes, SHA-256) then
//! `UpdateData`. The app writes them into a pipe the VMM forwards,
//! checked, to the guest (vmm/src/agent.rs); the agent checks every
//! file against the manifest before it installs anything (agent/src/update.rs).

use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use vroom_proto::{drop_path_ok, update_files_ok, Message, UpdateFile, UPDATE_CHUNK};

/// The files of `dir`, all the way down, as (path in the bundle, file):
/// in a fixed order, without hidden files or links.
pub fn files(dir: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    let mut todo = vec![(String::new(), dir.to_path_buf())];
    while let Some((rel, d)) = todo.pop() {
        let mut names: Vec<_> = std::fs::read_dir(&d)?.collect::<Result<_, _>>()?;
        names.sort_by_key(|e| e.file_name());
        for e in names {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let r = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            let t = e.file_type()?;
            if t.is_dir() {
                todo.push((r, e.path()));
            } else if t.is_file() {
                out.push((r, e.path()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The manifest for `files` (reads each once for its hash).
pub fn manifest(files: &[(String, PathBuf)]) -> io::Result<Vec<UpdateFile>> {
    let mut m = Vec::with_capacity(files.len());
    let mut buf = vec![0u8; 1 << 20];
    for (rel, p) in files {
        if !drop_path_ok(rel) {
            return Err(io::Error::other(format!("{rel}: a name the guest can't take")));
        }
        let mut f = std::fs::File::open(p)?;
        let mut h = Sha256::new();
        let mut size = 0u64;
        loop {
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
            size += n as u64;
        }
        m.push(UpdateFile { path: rel.clone(), size, sha256: h.finalize().into() });
    }
    if !update_files_ok(&m) {
        return Err(io::Error::other("too many files, or too big, for an update"));
    }
    Ok(m)
}

/// Write the update of `dir` (version `version`) as agent frames for
/// update `id`. Returns how many bytes of files went.
pub fn write(w: &mut dyn Write, id: u32, version: &str, dir: &Path) -> io::Result<u64> {
    let files = files(dir)?;
    let manifest = manifest(&files)?;
    w.write_all(&Message::UpdateBegin { id, version: version.to_string(), files: manifest.clone() }.encode())?;
    let mut sent = 0u64;
    let mut buf = vec![0u8; UPDATE_CHUNK];
    for ((_, p), m) in files.iter().zip(&manifest) {
        let mut f = std::fs::File::open(p)?.take(m.size);
        let mut left = m.size;
        while left > 0 {
            let n = f.read(&mut buf[..(left.min(UPDATE_CHUNK as u64) as usize)])?;
            if n == 0 {
                // Shorter than when it was hashed: the guest will refuse it.
                return Err(io::Error::other(format!("{}: changed while being sent", p.display())));
            }
            w.write_all(&Message::UpdateData { id, data: buf[..n].to_vec() }.encode())?;
            left -= n as u64;
            sent += n as u64;
        }
    }
    w.flush()?;
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vroom_proto::Decoder;

    #[test]
    fn a_folder_becomes_frames() {
        let t = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(t.path().join("gnome-shell")).unwrap();
        std::fs::write(t.path().join("vroom-agent"), vec![5u8; UPDATE_CHUNK + 10]).unwrap();
        std::fs::write(t.path().join("gnome-shell/extension.js"), b"js").unwrap();
        std::fs::write(t.path().join(".DS_Store"), b"no").unwrap();
        let mut out = Vec::new();
        let n = write(&mut out, 4, "0.1.0+1.ab", t.path()).unwrap();
        assert_eq!(n, UPDATE_CHUNK as u64 + 12);
        let mut d = Decoder::new(vroom_proto::MAX_PAYLOAD);
        d.push(&out);
        let Some(Message::UpdateBegin { id: 4, files, .. }) = d.next().unwrap() else { panic!() };
        let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(names, ["gnome-shell/extension.js", "vroom-agent"]);
        assert_eq!(files[0].sha256, <[u8; 32]>::from(Sha256::digest(b"js")));
        let mut data = Vec::new();
        while let Some(m) = d.next().unwrap() {
            let Message::UpdateData { id: 4, data: c } = m else { panic!() };
            assert!(c.len() <= UPDATE_CHUNK);
            data.extend(c);
        }
        assert_eq!(data.len() as u64, n);
        assert_eq!(&data[..2], b"js");
    }
}

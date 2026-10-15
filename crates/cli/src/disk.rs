//! `bulwark disk ...`

use std::path::PathBuf;

use clap::Subcommand;

#[derive(Subcommand, Debug)]
pub enum DiskCmd {
    /// Create a qcow2 image, optionally as an overlay on a base image
    /// (the base is never written to).
    Create {
        path: PathBuf,
        /// Size, e.g. 20G, 512M. Defaults to the backing image size.
        #[arg(long)]
        size: Option<String>,
        #[arg(long)]
        backing: Option<PathBuf>,
    },
    /// Show image info.
    Info { path: PathBuf },
    /// Rewrite a qcow2 image without the space freed inside it (the VM
    /// must be off).
    Compact { path: PathBuf },
}

pub(crate) fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let (num, mul) = match s.chars().last() {
        Some('K' | 'k') => (&s[..s.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&s[..s.len() - 1], 1 << 20),
        Some('G' | 'g') => (&s[..s.len() - 1], 1 << 30),
        Some('T' | 't') => (&s[..s.len() - 1], 1 << 40),
        _ => (s, 1),
    };
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mul))
        .ok_or_else(|| format!("bad size {s:?}"))
}

pub fn run(cmd: DiskCmd) -> Result<(), String> {
    use bulwark_block::{qcow2::Qcow2Image, Format};
    match cmd {
        DiskCmd::Create { path, size, backing } => {
            let b = match &backing {
                Some(p) => {
                    let fmt = bulwark_block::detect(p).map_err(|e| e.to_string())?;
                    let img = bulwark_block::open(p, Some(fmt), true).map_err(|e| e.to_string())?;
                    Some((p.as_path(), fmt, img.size()))
                }
                None => None,
            };
            let size = match (size, &b) {
                (Some(s), _) => parse_size(&s)?,
                (None, Some((_, _, sz))) => *sz,
                (None, None) => return Err("--size needed without --backing".into()),
            };
            Qcow2Image::create(&path, size, b.map(|(p, f, _)| (p, f))).map_err(|e| e.to_string())?;
            println!("created {} ({} bytes)", path.display(), size);
            Ok(())
        }
        DiskCmd::Compact { path } => {
            let (before, after) =
                bulwark_block::qcow2::compact(&path, &bulwark_block::qcow2::OpenPolicy::default()).map_err(|e| format!("{}: {e}", path.display()))?;
            println!("{}: {} -> {} bytes", path.display(), before, after);
            Ok(())
        }
        DiskCmd::Info { path } => {
            let fmt = bulwark_block::detect(&path).map_err(|e| e.to_string())?;
            let img = bulwark_block::open(&path, Some(fmt), true).map_err(|e| e.to_string())?;
            println!("format: {}", if fmt == Format::Qcow2 { "qcow2" } else { "raw" });
            println!("virtual size: {} bytes", img.size());
            println!("{img:?}");
            Ok(())
        }
    }
}


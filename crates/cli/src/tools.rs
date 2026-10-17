//! `vroom guest-tools`: where the guest tools are, their versions, and
//! an update for a running agent as a file (what the app sends; tests).

use std::path::PathBuf;

use clap::Subcommand;
use vroom_tools::Os;

#[derive(Debug, Subcommand)]
pub enum ToolsCmd {
    /// Where the guest tools are and which versions they hold.
    Info,
    /// Write one system's tools as an update for a running agent (agent
    /// frames, as the app sends them).
    Bundle {
        /// linux or windows.
        os: String,
        out: PathBuf,
    },
}

pub fn run(c: ToolsCmd) -> Result<(), String> {
    match c {
        ToolsCmd::Info => {
            let dir = vroom_tools::dir().ok_or("no guest tools here: build them with scripts/make-guest-tools.sh")?;
            println!("{}", dir.display());
            for os in [Os::Linux, Os::Windows] {
                match vroom_tools::version(os) {
                    Some(v) => println!("{}: {v}", os.dir_name()),
                    None => println!("{}: not built", os.dir_name()),
                }
            }
            match vroom_tools::image() {
                Some(p) => println!("image: {}", p.display()),
                None => println!("image: not built"),
            }
            Ok(())
        }
        ToolsCmd::Bundle { os, out } => {
            let os = Os::from_name(&os).ok_or("linux or windows")?;
            let dir = vroom_tools::os_dir(os).ok_or("those tools aren't built")?;
            let v = vroom_tools::version(os).ok_or("no version in the agent")?;
            let mut f = std::fs::File::create(&out).map_err(|e| format!("{}: {e}", out.display()))?;
            let n = vroom_tools::bundle::write(&mut f, 1, &v.to_string(), &dir).map_err(|e| e.to_string())?;
            println!("{}: {} {v}, {n} bytes of files", out.display(), os.dir_name());
            Ok(())
        }
    }
}

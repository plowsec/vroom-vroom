#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus};
use vroom_sandbox::Policy;

fn child(mode: &str, test: &str) -> ExitStatus {
    let dir = tempfile::tempdir().unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture"])
        .env("VROOM_IPC_CHILD", mode)
        .env("VROOM_IPC_DIR", dir.path())
        .status()
        .unwrap()
}

#[test]
fn preopened_ipc_accept_and_wake_survive_seccomp() {
    if std::env::var("VROOM_IPC_CHILD").as_deref() != Ok("accept") {
        assert!(child("accept", "preopened_ipc_accept_and_wake_survive_seccomp").success());
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("VROOM_IPC_DIR").unwrap());
    let listener = UnixListener::bind(root.join("viewer")).unwrap();
    let _client = UnixStream::connect(root.join("viewer")).unwrap();
    let (mut wake, mut receiver) = UnixStream::pair().unwrap();
    vroom_sandbox::apply(&Policy {
        write_dirs: vec![root],
        accept_preopened: true,
        ..Default::default()
    })
    .unwrap();
    let _viewer = listener.accept().unwrap();
    wake.write_all(&[1]).unwrap();
    let mut byte = [0];
    receiver.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [1]);
    std::process::exit(0);
}

#[test]
fn ipc_accept_permission_does_not_allow_new_unix_connections() {
    if std::env::var("VROOM_IPC_CHILD").as_deref() != Ok("deny") {
        assert_eq!(
            child(
                "deny",
                "ipc_accept_permission_does_not_allow_new_unix_connections"
            )
            .signal(),
            Some(libc::SIGSYS)
        );
        return;
    }
    let root = std::path::PathBuf::from(std::env::var_os("VROOM_IPC_DIR").unwrap());
    let _listener = UnixListener::bind(root.join("viewer")).unwrap();
    vroom_sandbox::apply(&Policy {
        write_dirs: vec![root.clone()],
        accept_preopened: true,
        ..Default::default()
    })
    .unwrap();
    let _forbidden = UnixStream::connect(root.join("viewer"));
    // The syscall filter must kill this process before it gets here.
    std::process::exit(99);
}

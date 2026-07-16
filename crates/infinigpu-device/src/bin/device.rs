//! `infinigpu-device --socket <path> [--vm-id <id>]`
//!
//! Spawned by infinization before QEMU; serves the vfio-user PCI device on a UNIX
//! socket for the lifetime of one VM. QEMU attaches with
//! `-device vfio-user-pci,socket=<path>`.

use std::path::PathBuf;

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut socket: Option<String> = None;
    let mut vm_id: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket" | "-s" => socket = args.next(),
            "--vm-id" => vm_id = args.next(),
            other => {
                eprintln!("infinigpu-device: unknown argument {other:?}");
                std::process::exit(2);
            }
        }
    }

    let Some(socket) = socket else {
        eprintln!("usage: infinigpu-device --socket <path> [--vm-id <id>]");
        std::process::exit(2);
    };

    let path = PathBuf::from(&socket);
    // Server::new refuses to bind if the path already exists.
    let _ = std::fs::remove_file(&path);
    eprintln!(
        "infinigpu-device: serving vfio-user on {socket}{}",
        vm_id.map(|id| format!(" (vm {id})")).unwrap_or_default()
    );

    if let Err(e) = infinigpu_device::serve(&path) {
        eprintln!("infinigpu-device: {e}");
        std::process::exit(1);
    }
}

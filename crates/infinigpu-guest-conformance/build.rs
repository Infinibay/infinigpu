//! Compile the freestanding C reference of the guest's PR4 ring producer + RESOURCE_* payload
//! builders (`csrc/guest_ring_ref.c`), including the generated wire-ABI header so the C uses the
//! real struct layouts. The companion `tests/interop.rs` links against it and drives the tested
//! Rust device consumer over its output.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    // guest/include/infinigpu_abi.h lives at the repo root (../../guest/include from this crate).
    let abi_include = manifest.join("../../guest/include");
    // The forwarded-draw encoder is shared with the Mesa ICD and lives in guest/icd/.
    let icd_dir = manifest.join("../../guest/icd");

    println!("cargo:rerun-if-changed=csrc/guest_ring_ref.c");
    println!("cargo:rerun-if-changed={}/infinigpu_abi.h", abi_include.display());
    println!("cargo:rerun-if-changed={}/infinigpu_forwarded.c", icd_dir.display());
    println!("cargo:rerun-if-changed={}/infinigpu_forwarded.h", icd_dir.display());

    cc::Build::new()
        .file("csrc/guest_ring_ref.c")
        // The exact forwarded-draw encoder the Mesa ICD's driver_submit will call, compiled here so
        // the interop test proves its bytes decode through the host's tested decoder — no guest VM.
        .file(icd_dir.join("infinigpu_forwarded.c"))
        .include(&abi_include)
        .include(&icd_dir)
        .flag_if_supported("-std=c11")
        .warnings(true)
        .compile("guest_ring_ref");
}

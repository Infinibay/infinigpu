//! End-to-end **host pipeline** demo (Phase-0 capstone). Drives the real device
//! backend in-process — no vfio-user socket, no guest OS — to prove the full chain:
//!
//! ```text
//!   guest RAM (memfd)                    infinigpu-device                 A5000
//!   ┌─────────────────┐  DMA_MAP        ┌──────────────────┐            ┌──────┐
//!   │ Descriptor      │───────────────► │ IOVA→HVA table    │            │      │
//!   │ SubmitCmd       │  doorbell ─────►│ decode SUBMIT_CMD │──render──► │ GPU  │
//!   │ ClearPresent    │                 │ (infinigpu-abi)   │            │      │
//!   │ …scanout buffer │◄── DMA write ───│ replay + present  │◄──frame────│      │
//!   └─────────────────┘                 └──────────────────┘            └──────┘
//! ```
//!
//! The guest submits a `DISPLAY_CLEAR`; the device reads it from guest RAM through
//! its DMA table, renders on the physical GPU (infinigpu-replay), and DMA-writes the
//! pixels back into the guest scanout buffer, raising the completion MSI-X. We then
//! verify the pixels landed in "guest RAM" and save the frame.

use infinigpu_abi::regs;
use infinigpu_abi::wire::{desc_flags, encoding, msg_type, ClearPresent, Descriptor, SubmitCmd};
use infinigpu_device::InfinigpuBackend;
use infinigpu_replay::Frame;
use std::fs::File;
use std::os::unix::io::{FromRawFd, RawFd};
use vfio_bindings::bindings::vfio::{
    VFIO_IRQ_SET_ACTION_TRIGGER, VFIO_IRQ_SET_DATA_EVENTFD, VFIO_PCI_BAR0_REGION_INDEX,
    VFIO_PCI_MSIX_IRQ_INDEX,
};
use vfio_user::{DmaMapFlags, ServerBackend};
use zerocopy::IntoBytes;

const GUEST_BASE: u64 = 0x8000_0000;
const RAM: usize = 8 << 20;
const RING_OFF: u64 = 0x1000;
const SCANOUT_OFF: u64 = 0x10_0000;
const W: u32 = 256;
const H: u32 = 256;
const BAR0: u32 = VFIO_PCI_BAR0_REGION_INDEX;

fn wr(backend: &mut InfinigpuBackend, off: u64, val: u32) {
    backend.region_write(BAR0, off, &val.to_le_bytes()).unwrap();
}

fn main() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // ---- allocate "guest RAM" and map it ----
    let ram_fd: RawFd = unsafe { libc::memfd_create(c"guestram".as_ptr(), 0) };
    assert!(ram_fd >= 0, "memfd_create failed");
    assert_eq!(unsafe { libc::ftruncate(ram_fd, RAM as libc::off_t) }, 0);
    let ram = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            RAM,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            ram_fd,
            0,
        )
    } as *mut u8;
    assert_ne!(ram as *mut libc::c_void, libc::MAP_FAILED, "mmap failed");

    // ---- the guest builds a one-entry command ring ----
    let ring_base = GUEST_BASE + RING_OFF;
    let payload_off: u32 = (size_of::<Descriptor>() + size_of::<SubmitCmd>()) as u32; // 72
    let desc = Descriptor {
        msg_type: msg_type::SUBMIT_CMD,
        flags: desc_flags::FENCED,
        len: size_of::<ClearPresent>() as u32,
        data_offset: payload_off,
        seqno: 1,
        payload_addr: 0,
    };
    let submit = SubmitCmd {
        ctx_id: 0,
        encoding: encoding::DISPLAY_CLEAR,
        payload_len: size_of::<ClearPresent>() as u32,
        flags: 0,
        seqno: 1,
        in_fence: 0,
        out_fence: 1,
    };
    let clear = ClearPresent {
        width: W,
        height: H,
        rgba: [0.0, 0.6, 0.8, 1.0], // → 8-bit [0, 153, 204, 255]
        scanout_addr: GUEST_BASE + SCANOUT_OFF,
    };
    unsafe {
        let put = |off: u64, bytes: &[u8]| {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ram.add(off as usize), bytes.len())
        };
        put(RING_OFF, desc.as_bytes());
        put(RING_OFF + size_of::<Descriptor>() as u64, submit.as_bytes());
        put(RING_OFF + payload_off as u64, clear.as_bytes());
    }

    // ---- the device side ----
    let mut backend = InfinigpuBackend::new();

    // DMA_MAP all guest RAM (dup the fd so the backend owns its own copy).
    let dup = unsafe { libc::dup(ram_fd) };
    let file = unsafe { File::from_raw_fd(dup) };
    backend
        .dma_map(
            DmaMapFlags::READ_WRITE,
            0,
            GUEST_BASE,
            RAM as u64,
            Some(file),
        )
        .expect("dma_map");

    // completion eventfd on command-ring-0's vector (MSI-X vector 1).
    let done: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
    backend
        .set_irqs(
            VFIO_PCI_MSIX_IRQ_INDEX,
            VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
            1,
            1,
            vec![unsafe { File::from_raw_fd(libc::dup(done)) }],
        )
        .expect("set_irqs");

    // program command-ring 0 base, then ring its doorbell (runs the submit engine).
    wr(
        &mut backend,
        regs::ctrl::CMD_RING_CFG + regs::ctrl::CMD_RING_BASE_LO,
        ring_base as u32,
    );
    wr(
        &mut backend,
        regs::ctrl::CMD_RING_CFG + regs::ctrl::CMD_RING_BASE_HI,
        (ring_base >> 32) as u32,
    );
    println!(">> guest rings command-ring-0 doorbell (submits DISPLAY_CLEAR)…");
    wr(&mut backend, regs::doorbell::CMD_BASE, 1);

    // ---- verify completion + the rendered pixels landed in guest RAM ----
    let mut evbuf = [0u8; 8];
    let signaled = unsafe { libc::read(done, evbuf.as_mut_ptr() as *mut libc::c_void, 8) } == 8;
    let n = (W * H * 4) as usize;
    let mut scanout = vec![0u8; n];
    unsafe {
        std::ptr::copy_nonoverlapping(ram.add(SCANOUT_OFF as usize), scanout.as_mut_ptr(), n);
    }
    let px0 = [scanout[0], scanout[1], scanout[2], scanout[3]];
    let expected = [0u8, 153, 204, 255];

    println!(">> completion MSI-X fired: {signaled}");
    println!(">> scanout[0,0] in guest RAM = {px0:?}  (expected {expected:?})");

    let ok = signaled && px0 == expected && scanout[n - 4..] == expected;
    if ok {
        let ppm = Frame {
            width: W,
            height: H,
            rgba: scanout,
        }
        .to_ppm();
        let out = "target/infinigpu-pipeline.ppm";
        std::fs::write(out, ppm).unwrap();
        println!(
            "\nOK — the guest's ring submission rendered on the GPU and the frame was\n\
             DMA-written back into guest RAM. Full pipeline verified. Saved {out}"
        );
    } else {
        eprintln!("\nFAIL — pipeline did not complete as expected");
        std::process::exit(1);
    }

    unsafe {
        libc::munmap(ram as *mut libc::c_void, RAM);
        libc::close(ram_fd);
        libc::close(done);
    }
}

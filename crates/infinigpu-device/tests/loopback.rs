//! In-process seam validation for `infinigpu-device` — no QEMU required.
//!
//! Runs our `ServerBackend` in a thread and drives it with the `vfio_user` `Client`
//! (the same protocol QEMU speaks), proving the Phase-0 Step-1 primitives:
//!   (a) config space returns our PCI identity + class,
//!   (b) BAR0 control registers trap and read/write correctly (MAGIC/ABI/CAPS),
//!   (c) **zero-copy guest-RAM DMA**: a memfd passed over the socket is mmap'd by
//!       the device and read/written through the IOVA table,
//!   (d) **MSI-X**: a doorbell write raises the matching per-vector eventfd.
//!
//! The ioeventfd doorbell is deliberately *not* tested — v0.1.3 doesn't support it
//! (POLL_SUBMIT is the model). Here a doorbell write stands in as the poller wake.

use infinigpu_abi::{abi_version, regs, DEV_MAGIC};
use infinigpu_device::dbg;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};
use vfio_bindings::bindings::vfio::{
    VFIO_IRQ_SET_ACTION_TRIGGER, VFIO_IRQ_SET_DATA_EVENTFD, VFIO_PCI_BAR0_REGION_INDEX,
    VFIO_PCI_CONFIG_REGION_INDEX, VFIO_PCI_MSIX_IRQ_INDEX,
};
use vfio_user::Client;

fn read_u32(client: &mut Client, region: u32, off: u64) -> u32 {
    let mut b = [0u8; 4];
    client.region_read(region, off, &mut b).unwrap();
    u32::from_le_bytes(b)
}

fn write_u32(client: &mut Client, region: u32, off: u64, v: u32) {
    client.region_write(region, off, &v.to_le_bytes()).unwrap();
}

#[test]
fn loopback_seam_smoke() {
    let path = std::env::temp_dir().join(format!("infinigpu-loopback-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);

    // ---- server in its own thread ----
    let server_path = path.clone();
    let server = std::thread::spawn(move || {
        // serve() blocks on accept, serves one connection, returns on disconnect.
        let _ = infinigpu_device::serve(&server_path);
    });

    // wait for the socket to appear
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "server socket never appeared");
        std::thread::sleep(Duration::from_millis(1));
    }

    let mut client = Client::new(&path).expect("client connect");

    // ---- (a) config space: PCI identity + display class ----
    let vendor = read_u32(&mut client, VFIO_PCI_CONFIG_REGION_INDEX, 0x00);
    assert_eq!(vendor & 0xFFFF, 0x1B36, "vendor id");
    assert_eq!(
        (vendor >> 16) & 0xFFFF,
        0x0110,
        "device id (must avoid QXL 0x0100)"
    );
    let class = read_u32(&mut client, VFIO_PCI_CONFIG_REGION_INDEX, 0x08);
    assert_eq!(
        class >> 8,
        0x0003_8000,
        "class code = display-other 0x038000"
    );

    // ---- (b) BAR0 control registers ----
    assert_eq!(
        read_u32(
            &mut client,
            VFIO_PCI_BAR0_REGION_INDEX,
            regs::ctrl::DEV_MAGIC
        ),
        DEV_MAGIC,
        "DEV_MAGIC"
    );
    assert_eq!(
        read_u32(
            &mut client,
            VFIO_PCI_BAR0_REGION_INDEX,
            regs::ctrl::ABI_VERSION
        ),
        abi_version(),
        "ABI_VERSION"
    );
    let caps = read_u32(
        &mut client,
        VFIO_PCI_BAR0_REGION_INDEX,
        regs::ctrl::DEV_CAPS,
    );
    // Phase-1: the device advertises the 2D damage path (DISPLAY_ACCEL) on top of Phase-0.
    assert_eq!(caps, regs::PHASE1_DEV_CAPS);
    assert!(
        caps & regs::caps::DISPLAY_ACCEL != 0,
        "must advertise DISPLAY_ACCEL (2D damage path)"
    );
    assert!(
        caps & regs::caps::POLL_SUBMIT != 0,
        "must advertise POLL_SUBMIT"
    );
    assert!(
        caps & regs::caps::IOEVENTFD_DOORBELL == 0,
        "must NOT claim ioeventfd"
    );
    // writable register round-trips
    write_u32(
        &mut client,
        VFIO_PCI_BAR0_REGION_INDEX,
        regs::ctrl::GLOBAL_CTRL,
        regs::global_ctrl::DEVICE_ENABLE,
    );
    assert_eq!(
        read_u32(
            &mut client,
            VFIO_PCI_BAR0_REGION_INDEX,
            regs::ctrl::GLOBAL_CTRL
        ),
        regs::global_ctrl::DEVICE_ENABLE,
        "GLOBAL_CTRL round-trip"
    );

    // ---- (c) zero-copy DMA through a shared memfd ----
    const GUEST_BASE: u64 = 0x8000_0000;
    const RAM_SIZE: usize = 0x10000;
    const PAT_OFF: usize = 0x2000;
    const PATTERN: u32 = 0xCAFE_B0BA;

    // create "guest RAM", map it ourselves, write a pattern the device should read.
    let ram_fd: RawFd = unsafe { libc::memfd_create(c"guestram".as_ptr(), 0) };
    assert!(ram_fd >= 0, "memfd_create");
    assert_eq!(
        unsafe { libc::ftruncate(ram_fd, RAM_SIZE as libc::off_t) },
        0
    );
    let ram = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            RAM_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            ram_fd,
            0,
        )
    };
    assert_ne!(ram, libc::MAP_FAILED, "mmap guest ram");
    unsafe {
        std::ptr::copy_nonoverlapping(
            PATTERN.to_le_bytes().as_ptr(),
            (ram as *mut u8).add(PAT_OFF),
            4,
        );
    }

    // hand the memfd to the device via DMA_MAP (SCM_RIGHTS over the socket).
    client
        .dma_map(0, GUEST_BASE, RAM_SIZE as u64, ram_fd)
        .expect("dma_map");

    // program the debug DMA address and read it back *through the device*.
    let addr = GUEST_BASE + PAT_OFF as u64;
    write_u32(
        &mut client,
        VFIO_PCI_BAR0_REGION_INDEX,
        dbg::DMA_ADDR_LO,
        addr as u32,
    );
    write_u32(
        &mut client,
        VFIO_PCI_BAR0_REGION_INDEX,
        dbg::DMA_ADDR_HI,
        (addr >> 32) as u32,
    );
    let seen = read_u32(&mut client, VFIO_PCI_BAR0_REGION_INDEX, dbg::DMA_DATA);
    assert_eq!(seen, PATTERN, "device read guest RAM zero-copy");

    // device writes back into guest RAM; confirm via our own mapping.
    const REPLY: u32 = 0x1234_5678;
    write_u32(
        &mut client,
        VFIO_PCI_BAR0_REGION_INDEX,
        dbg::DMA_DATA,
        REPLY,
    );
    let mut back = [0u8; 4];
    unsafe {
        std::ptr::copy_nonoverlapping((ram as *const u8).add(PAT_OFF), back.as_mut_ptr(), 4);
    }
    assert_eq!(
        u32::from_le_bytes(back),
        REPLY,
        "device wrote guest RAM zero-copy"
    );

    // ---- (d) MSI-X delivery via eventfds ----
    let ev0: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
    let ev1: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
    assert!(ev0 >= 0 && ev1 >= 0, "eventfd");
    client
        .set_irqs(
            VFIO_PCI_MSIX_IRQ_INDEX,
            VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
            0,
            2,
            &[ev0, ev1],
        )
        .expect("set_irqs");

    // control doorbell -> vector 0; command-ring-0 doorbell -> vector 1
    write_u32(
        &mut client,
        VFIO_PCI_BAR0_REGION_INDEX,
        regs::doorbell::CTRL,
        1,
    );
    assert_eq!(
        read_eventfd(ev0),
        Some(1),
        "MSI-X vector 0 raised by control doorbell"
    );
    write_u32(
        &mut client,
        VFIO_PCI_BAR0_REGION_INDEX,
        regs::doorbell::CMD_BASE,
        1,
    );
    assert_eq!(
        read_eventfd(ev1),
        Some(1),
        "MSI-X vector 1 raised by ring-0 doorbell"
    );
    // vector 0 must not have re-fired
    assert_eq!(read_eventfd(ev0), None, "vector 0 not spuriously raised");

    // ---- teardown ----
    client.shutdown().ok();
    server.join().ok();

    unsafe {
        libc::munmap(ram, RAM_SIZE);
        libc::close(ram_fd);
        libc::close(ev0);
        libc::close(ev1);
    }
}

/// Read an eventfd counter (non-blocking). `Some(n)` if signalled, `None` if empty.
fn read_eventfd(fd: RawFd) -> Option<u64> {
    let mut buf = [0u8; 8];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 8) };
    if n == 8 {
        Some(u64::from_ne_bytes(buf))
    } else {
        None
    }
}

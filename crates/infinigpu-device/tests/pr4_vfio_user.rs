//! PR4 end-to-end over the **real vfio-user wire protocol** — the same protocol QEMU speaks. This
//! is the off-hardware proxy for "runtime validation under QEMU" of the *device* side: a
//! `vfio_user::Client` (not an in-process shortcut) drives the device over a socket, exactly as
//! QEMU's vfio-user frontend would. It programs the real ring registers, DMA-maps guest RAM holding
//! a live SPSC ring + a `RESOURCE_*` stream (built with the same producer the guest `.ko` uses),
//! rings the doorbell, and verifies the device drained the ring, retired the fence, raised the
//! completion interrupt, and **actually presented the blob** (observed via the diagnostic PPM).
//!
//! Isolated in its own test binary so `INFINIGPU_PRESENT_DIR` is set cleanly for this process only.

use infinigpu_abi::regs::ctrl;
use infinigpu_abi::wire::{
    format, msg_type, AttachBacking, Descriptor, MemEntry, ResourceCreateBlob, ResourceFlush,
    SetScanoutBlob,
};
use infinigpu_device::drain::ring_over_shared;
use std::os::unix::io::RawFd;
use std::time::{Duration, Instant};
use vfio_bindings::bindings::vfio::{
    VFIO_IRQ_SET_ACTION_TRIGGER, VFIO_IRQ_SET_DATA_EVENTFD, VFIO_PCI_BAR0_REGION_INDEX,
    VFIO_PCI_MSIX_IRQ_INDEX,
};
use vfio_user::Client;
use zerocopy::IntoBytes;

fn read_u32(client: &mut Client, off: u64) -> u32 {
    let mut b = [0u8; 4];
    client.region_read(VFIO_PCI_BAR0_REGION_INDEX, off, &mut b).unwrap();
    u32::from_le_bytes(b)
}
fn write_u32(client: &mut Client, off: u64, v: u32) {
    client.region_write(VFIO_PCI_BAR0_REGION_INDEX, off, &v.to_le_bytes()).unwrap();
}
fn read_eventfd(fd: RawFd) -> Option<u64> {
    let mut buf = [0u8; 8];
    let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, 8) };
    (n == 8).then(|| u64::from_ne_bytes(buf))
}

#[test]
fn pr4_ring_drains_and_presents_over_vfio_user() {
    // Observe the present through the black-box client via the diagnostic PPM.
    let present_dir = std::env::temp_dir().join(format!("igpu-pr4-{}", std::process::id()));
    std::fs::create_dir_all(&present_dir).unwrap();
    let latest_ppm = present_dir.join("latest.ppm");
    let _ = std::fs::remove_file(&latest_ppm);
    // Safe: this test binary is a dedicated process; the server thread reads the env at startup.
    unsafe { std::env::set_var("INFINIGPU_PRESENT_DIR", &present_dir) };

    // ---- server over a socket (the real vfio-user transport) ----
    let path = std::env::temp_dir().join(format!("igpu-pr4-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let server_path = path.clone();
    let server = std::thread::spawn(move || {
        let _ = infinigpu_device::serve(&server_path);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        assert!(Instant::now() < deadline, "server socket never appeared");
        std::thread::sleep(Duration::from_millis(1));
    }
    let mut client = Client::new(&path).expect("client connect");
    write_u32(&mut client, ctrl::GLOBAL_CTRL, 1); // DEVICE_ENABLE

    // ---- guest RAM: a real ring + a RESOURCE_* stream + a blob framebuffer ----
    const GUEST_BASE: u64 = 0x8000_0000;
    const SIZE: usize = 0x4000;
    const IDX_OFF: usize = 0x000;
    const DESC_OFF: usize = 0x040;
    const PAY_OFF: usize = 0x400;
    const BLOB_OFF: usize = 0x2000;
    const CAP: usize = 8;
    let (w, h) = (4u32, 4u32);
    let stride = w * 4;
    let fb_bytes = (stride * h) as u64;

    let ram_fd: RawFd = unsafe { libc::memfd_create(c"pr4vfioram".as_ptr(), 0) };
    assert!(ram_fd >= 0);
    assert_eq!(unsafe { libc::ftruncate(ram_fd, SIZE as libc::off_t) }, 0);
    let ram = unsafe {
        libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, ram_fd, 0)
    } as *mut u8;
    assert_ne!(ram as *mut libc::c_void, libc::MAP_FAILED);

    // Known BGRA framebuffer (A=255) → PPM pixel i = (R=0x40+i, G=0x20+i, B=i+1).
    for i in 0..(w * h) as usize {
        let p = BLOB_OFF + i * 4;
        unsafe {
            *ram.add(p) = i as u8 + 1; // B
            *ram.add(p + 1) = 0x20 + i as u8; // G
            *ram.add(p + 2) = 0x40 + i as u8; // R
            *ram.add(p + 3) = 255; // A
        }
    }

    // Build the four RESOURCE_* payloads in guest RAM (data_offset is relative to the desc array).
    let write_at = |off: usize, bytes: &[u8]| unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ram.add(off), bytes.len());
    };
    let align16 = |n: usize| (n + 15) & !15;
    let o1 = PAY_OFF;
    write_at(o1, ResourceCreateBlob { res_id: 1, ctx_id: 0, blob_mem: 1, blob_flags: 0, size: fb_bytes }.as_bytes());
    let l1 = 24usize;
    let o2 = o1 + align16(l1);
    let mut ab = AttachBacking { res_id: 1, num_entries: 1 }.as_bytes().to_vec();
    ab.extend_from_slice(MemEntry { addr: GUEST_BASE + BLOB_OFF as u64, length: fb_bytes }.as_bytes());
    write_at(o2, &ab);
    let o3 = o2 + align16(ab.len());
    write_at(o3, SetScanoutBlob { scanout_id: 0, res_id: 1, width: w, height: h, format: format::B8G8R8A8, stride }.as_bytes());
    let l3 = 24usize;
    let o4 = o3 + align16(l3);
    write_at(o4, ResourceFlush { res_id: 1, x: 0, y: 0, w, h, _reserved: 0 }.as_bytes());
    let l4 = 24usize;

    // Publish the four descriptors through the SPSC producer over the shared page.
    {
        let ring = unsafe { ring_over_shared(ram.add(IDX_OFF), ram.add(DESC_OFF), CAP) }.unwrap();
        for (i, &(mt, off, len)) in [
            (msg_type::RESOURCE_CREATE_BLOB, o1, l1),
            (msg_type::RESOURCE_ATTACH_BACKING, o2, ab.len()),
            (msg_type::SET_SCANOUT_BLOB, o3, l3),
            (msg_type::RESOURCE_FLUSH, o4, l4),
        ]
        .iter()
        .enumerate()
        {
            ring.push(Descriptor {
                msg_type: mt,
                flags: 0,
                len: len as u32,
                data_offset: (off - DESC_OFF) as u32,
                seqno: (i + 1) as u64,
                _reserved: 0,
            })
            .unwrap();
        }
    }

    // ---- hand guest RAM to the device + program the real ring over BAR0 ----
    client.dma_map(0, GUEST_BASE, SIZE as u64, ram_fd).expect("dma_map");
    let blk = |field: u64| ctrl::CMD_RING_CFG + field; // ctx 0
    let desc_iova = GUEST_BASE + DESC_OFF as u64;
    let idx_iova = GUEST_BASE + IDX_OFF as u64;
    write_u32(&mut client, blk(ctrl::CMD_RING_BASE_LO), desc_iova as u32);
    write_u32(&mut client, blk(ctrl::CMD_RING_BASE_HI), (desc_iova >> 32) as u32);
    write_u32(&mut client, blk(ctrl::CMD_RING_SIZE), CAP as u32);
    write_u32(&mut client, blk(ctrl::CMD_RING_INDEX_LO), idx_iova as u32);
    write_u32(&mut client, blk(ctrl::CMD_RING_INDEX_HI), (idx_iova >> 32) as u32);

    // MSI-X vectors (0 = control, 1 = ring 0 completion).
    let ev0: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
    let ev1: RawFd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK) };
    client
        .set_irqs(
            VFIO_PCI_MSIX_IRQ_INDEX,
            VFIO_IRQ_SET_DATA_EVENTFD | VFIO_IRQ_SET_ACTION_TRIGGER,
            0,
            2,
            &[ev0, ev1],
        )
        .expect("set_irqs");

    // ---- ring the doorbell (the device drains synchronously in this non-posted write) ----
    write_u32(&mut client, infinigpu_abi::regs::doorbell::CMD_BASE, 1);

    // ---- assertions: drained, retired, interrupted, presented ----
    assert_eq!(read_eventfd(ev1), Some(1), "ring-0 completion MSI-X raised");
    let retired = read_u32(&mut client, ctrl::CMD_RING0_RETIRED_LO);
    assert_eq!(retired, 4, "device retired all 4 descriptors over the wire");

    // The shared index page shows the ring fully drained (head advanced to tail).
    {
        let ring = unsafe { ring_over_shared(ram.add(IDX_OFF), ram.add(DESC_OFF), CAP) }.unwrap();
        assert!(ring.is_empty(), "head == tail: device drained the whole ring");
        assert_eq!(ring.retired(), 4, "seqno_retired published on the shared page");
    }

    // The RESOURCE_FLUSH actually presented: the diagnostic PPM matches the blob.
    let ppm = std::fs::read(&latest_ppm).expect("device wrote latest.ppm (present happened)");
    let hdr = format!("P6\n{w} {h}\n255\n");
    assert!(ppm.starts_with(hdr.as_bytes()), "PPM header");
    let px = &ppm[hdr.len()..];
    assert_eq!(px.len(), (w * h * 3) as usize);
    for i in 0..(w * h) as usize {
        assert_eq!(px[i * 3], 0x40 + i as u8, "R of pixel {i}");
        assert_eq!(px[i * 3 + 1], 0x20 + i as u8, "G of pixel {i}");
        assert_eq!(px[i * 3 + 2], i as u8 + 1, "B of pixel {i}");
    }

    // ---- teardown ----
    client.shutdown().ok();
    server.join().ok();
    unsafe {
        libc::munmap(ram as *mut libc::c_void, SIZE);
        libc::close(ram_fd);
        libc::close(ev0);
        libc::close(ev1);
    }
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_dir_all(&present_dir);
}

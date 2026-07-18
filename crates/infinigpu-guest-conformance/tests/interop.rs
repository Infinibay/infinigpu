//! Cross-language PR4 interop: the **guest** builds a real SPSC ring + a `RESOURCE_*` stream using
//! the C reference producer (`csrc/guest_ring_ref.c`, the exact logic the `.ko` uses); the **host**
//! drains it with the *tested* Rust device consumer (`infinigpu_device::drain` +
//! `dispatch::execute_resource`). If the guest's wire layout, ring-index math, or payload encoding
//! disagreed with the device by a single byte, the drain would surface it (a dropped descriptor, a
//! `ShortPayload`, a wrong seqno, or a `Rejected`). Proving it end-to-end here verifies the guest
//! half of PR4 that a `cargo`-side unit test otherwise can't — entirely off-hardware, no QEMU.

use infinigpu_device::dispatch::{execute_resource, Executed};
use infinigpu_device::drain::{pop_batch, ring_over_shared, retire_over_shared};
use infinigpu_device::resource::ResourceTable;
use infinigpu_guest_conformance as guest;

// Wire constants (mirror infinigpu_abi::wire).
const MSG_RESOURCE_CREATE_BLOB: u32 = 0x0020;
const MSG_RESOURCE_ATTACH_BACKING: u32 = 0x0021;
const MSG_SET_SCANOUT_BLOB: u32 = 0x0040;
const MSG_RESOURCE_FLUSH: u32 = 0x0041;
const FMT_B8G8R8A8: u32 = 1;
const DESC_SIZE: usize = 32;

/// A single owned buffer partitioned like guest RAM: [index page][descriptor array][payloads][blob].
struct GuestRam {
    buf: Vec<u8>,
}

const IDX_OFF: usize = 0x000;
const DESC_OFF: usize = 0x040;
const PAY_OFF: usize = 0x400;
const BLOB_OFF: usize = 0x2000;
const CAP: u32 = 8;

impl GuestRam {
    fn new() -> Self {
        GuestRam { buf: vec![0u8; 0x4000] }
    }
    fn base(&mut self) -> *mut u8 {
        self.buf.as_mut_ptr()
    }
}

#[test]
fn guest_ring_producer_interops_with_the_device_consumer() {
    let mut ram = GuestRam::new();
    let (w, h) = (4u32, 4u32);
    let stride = w * 4;
    let fb_bytes = (stride * h) as u64;

    // A known BGRA framebuffer in the blob region (the guest's dumb FB).
    for i in 0..(w * h) as usize {
        let p = BLOB_OFF + i * 4;
        ram.buf[p] = i as u8 + 1; // B
        ram.buf[p + 1] = 0x20 + i as u8; // G
        ram.buf[p + 2] = 0x40 + i as u8; // R
        ram.buf[p + 3] = 255; // A
    }
    let blob_iova = BLOB_OFF as u64; // this test's "guest physical" == buffer offset

    let base = ram.base();
    let idx_base = unsafe { base.add(IDX_OFF) };
    let desc_base = unsafe { base.add(DESC_OFF) };

    // ---- GUEST: build the four RESOURCE_* payloads + push a descriptor for each ----
    // `data_offset` is relative to the descriptor array base (the device's convention). A
    // side-effect-free helper (captures only Copy raw pointers) pushes at an explicit offset; we
    // advance the payload cursor ourselves, 16-aligned.
    let push_msg = |msg_type: u32, off: usize, body_len: u32| -> u64 {
        let data_offset = (off - DESC_OFF) as u32;
        unsafe { guest::push(idx_base, desc_base, CAP, msg_type, data_offset, body_len) }
    };
    let align16 = |n: u32| ((n as usize) + 15) & !15;

    let off1 = PAY_OFF;
    let create_len = unsafe { guest::create_blob(base.add(off1), 1, fb_bytes) };
    let s1 = push_msg(MSG_RESOURCE_CREATE_BLOB, off1, create_len);
    let off2 = off1 + align16(create_len);
    let attach_len = unsafe { guest::attach_backing(base.add(off2), 1, blob_iova, fb_bytes) };
    let s2 = push_msg(MSG_RESOURCE_ATTACH_BACKING, off2, attach_len);
    let off3 = off2 + align16(attach_len);
    let scanout_len = unsafe { guest::set_scanout(base.add(off3), 0, 1, w, h, FMT_B8G8R8A8, stride) };
    let s3 = push_msg(MSG_SET_SCANOUT_BLOB, off3, scanout_len);
    let off4 = off3 + align16(scanout_len);
    let flush_len = unsafe { guest::flush(base.add(off4), 1, 0, 0, w, h) };
    let s4 = push_msg(MSG_RESOURCE_FLUSH, off4, flush_len);
    assert_eq!((s1, s2, s3, s4), (1, 2, 3, 4), "SPSC seqnos are 1-based and monotonic");

    // ---- HOST: drain with the tested device consumer, dispatch into a per-VM ResourceTable ----
    let drained = {
        let ring = unsafe { ring_over_shared(idx_base, desc_base, CAP as usize) }.unwrap();
        pop_batch(&ring, CAP as usize)
    };
    assert_eq!(drained.descriptors.len(), 4, "device drained all guest-published descriptors");
    assert_eq!(drained.highest_seqno, 4);

    let mut table = ResourceTable::new();
    let mut outcomes = Vec::new();
    for d in &drained.descriptors {
        // Payload at ring_base + data_offset, exactly as the device resolves it.
        let start = DESC_OFF + d.data_offset as usize;
        let payload = &ram.buf[start..start + d.len as usize];
        outcomes.push(execute_resource(d, payload, &mut table));
    }

    // Each guest-built message decoded to the intended device-side effect — byte-level interop.
    assert_eq!(outcomes[0], Executed::CreatedBlob(1), "CREATE_BLOB round-tripped");
    assert_eq!(
        outcomes[1],
        Executed::AttachedBacking { res_id: 1, segments: 1 },
        "ATTACH_BACKING round-tripped (single segment)"
    );
    assert_eq!(outcomes[2], Executed::SetScanout(0), "SET_SCANOUT_BLOB round-tripped");
    assert_eq!(
        outcomes[3],
        Executed::Flush { res_id: 1, rect: (0, 0, w, h) },
        "RESOURCE_FLUSH routed to a present with the guest's rect"
    );

    // The resource table reflects the guest's intent.
    assert!(table.get(1).is_some());
    assert_eq!(table.scanout_binding_for(1).unwrap().0, 0);

    // ---- HOST retires; GUEST observes it (the fence-resolution path) ----
    unsafe { retire_over_shared(idx_base, desc_base, CAP as usize, drained.highest_seqno) }.unwrap();
    let seen = unsafe { guest::retired(idx_base) };
    assert_eq!(seen, 4, "guest reads back the host-retired seqno to resolve its fences");

    // A fresh view confirms the ring fully drained (head advanced to tail).
    let ring = unsafe { ring_over_shared(idx_base, desc_base, CAP as usize) }.unwrap();
    assert!(ring.is_empty());
    // Descriptor stride sanity (the guest slot size the device expects).
    assert_eq!(DESC_SIZE, 32);
}

#[test]
fn guest_ring_producer_respects_capacity_like_the_consumer() {
    // The C producer must reject once `cap` unretired descriptors are outstanding — same full
    // condition the Rust Ring enforces — so guest and host agree on backpressure.
    let mut ram = GuestRam::new();
    let base = ram.base();
    let idx_base = unsafe { base.add(IDX_OFF) };
    let desc_base = unsafe { base.add(DESC_OFF) };
    for i in 0..CAP {
        let s = unsafe { guest::push(idx_base, desc_base, CAP, MSG_RESOURCE_FLUSH, 0, 0) };
        assert_eq!(s, (i + 1) as u64);
    }
    // Ring is full → next push is refused (seqno 0), not a silent overwrite.
    assert_eq!(unsafe { guest::push(idx_base, desc_base, CAP, MSG_RESOURCE_FLUSH, 0, 0) }, 0);

    // The device drains all CAP, and now the guest can push again.
    {
        let ring = unsafe { ring_over_shared(idx_base, desc_base, CAP as usize) }.unwrap();
        let drained = pop_batch(&ring, CAP as usize);
        assert_eq!(drained.descriptors.len(), CAP as usize);
    }
    assert_ne!(unsafe { guest::push(idx_base, desc_base, CAP, MSG_RESOURCE_FLUSH, 0, 0) }, 0);
}

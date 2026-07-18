//! Two-phase bounded ring drainer — the reusable core of 2D-ADR **PR4** (and the 3D
//! Phase-1 host ring drainer). This is the piece the ADR flags as the *biggest structural
//! risk*: running the loom-verified [`infinigpu_ring::Ring`] SPSC protocol directly over the
//! guest-shared index page (`Indices::from_ptr`, a `repr(C,align(64))` view byte-identical to
//! `wire::RingIndices`) and splitting the drain into two borrow phases so descriptor execution
//! can take `&mut self` without holding the ring borrow (which aliases DMA-mapped guest memory).
//!
//! It is deliberately **transport-agnostic and side-effect-free**: it pops descriptors and
//! reports the highest seqno; the caller executes them and retires the fence. That keeps the
//! risky borrow-splitting logic fully unit-testable off-hardware (plain owned buffers stand in
//! for the sparse-mmap'd index page), decoupled from the QEMU-gated vfio-user region transport.
//!
//! ## The two phases (why)
//!
//! In the device, the [`Ring`] view borrows `self.dma`-mapped memory (the index page + the
//! descriptor array in guest RAM), while executing a descriptor needs `&mut self` (DMA reads,
//! resource-table mutation, present). Those borrows conflict. So the drain is:
//!
//!   1. **Phase 1 ([`pop_batch`]):** build the `Ring`, pop up to `max` descriptors into an owned
//!      batch, note the highest seqno, then **drop the `Ring`** — releasing the DMA borrow.
//!   2. **Phase 2 (caller):** execute each descriptor under `&mut self`.
//!   3. **Retire ([`retire_over_shared`]):** briefly re-view the ring and publish the highest
//!      seqno so the guest's fences resolve.
//!
//! Bounding phase 1 at `max` (== ring capacity) guarantees forward progress and a hostile
//! producer can never make one drain unbounded.

use infinigpu_abi::wire::Descriptor;
use infinigpu_ring::{Indices, Ring, RingError, Slot};

/// Result of phase 1: the descriptors popped (in FIFO order) and the highest seqno among them
/// (0 if none). The caller executes `descriptors`, then retires `highest_seqno`.
#[derive(Debug, Default)]
pub struct Drained {
    pub descriptors: Vec<Descriptor>,
    pub highest_seqno: u64,
}

/// **Phase 1** of the bounded drain: pop up to `max` descriptors from `ring` into an owned batch,
/// FIFO. Stops early when the ring drains. `max` bounds the work of a single drain (pass the ring
/// capacity) so a hostile producer that keeps publishing cannot make one call unbounded — the next
/// poll picks up the rest. The `Ring` borrow is released when this returns, so the caller is then
/// free to execute the batch under `&mut self`.
pub fn pop_batch(ring: &Ring<'_, Descriptor>, max: usize) -> Drained {
    let cap = ring.capacity() as usize;
    let bound = max.min(cap);
    let mut out = Drained {
        descriptors: Vec::with_capacity(bound.min(ring.len() as usize)),
        highest_seqno: 0,
    };
    while out.descriptors.len() < bound {
        match ring.pop() {
            Some(d) => {
                // seqno is monotonic with the producer index, but take the max defensively —
                // the descriptor slot is guest-writable (TOCTOU) and we never trust its order.
                if d.seqno > out.highest_seqno {
                    out.highest_seqno = d.seqno;
                }
                out.descriptors.push(d);
            }
            None => break,
        }
    }
    out
}

/// Build a [`Ring`] view over the guest-shared index page + descriptor array. `cap` must be a
/// non-zero power of two (checked by [`Ring::from_parts`]).
///
/// # Safety
/// - `index_ptr` must satisfy [`Indices::from_ptr`]'s contract: ≥64 readable/writable bytes laid
///   out as `wire::RingIndices`, 64-aligned, valid for `'a`, and the sole typed view.
/// - `desc_ptr` must point to `cap` contiguous `Slot<Descriptor>` (== `cap * size_of::<Descriptor>()`
///   bytes) readable for `'a`. `Slot<Descriptor>` is `repr(transparent)` over the descriptor, so a
///   guest descriptor array is a valid slot array.
/// - Nothing else may write the index page / descriptor array as a different type for `'a`.
///
/// The returned view is only valid while the underlying guest mapping stays mapped.
pub unsafe fn ring_over_shared<'a>(
    index_ptr: *const u8,
    desc_ptr: *const u8,
    cap: usize,
) -> Result<Ring<'a, Descriptor>, RingError> {
    let idx: &'a Indices = Indices::from_ptr(index_ptr);
    let slots: &'a [Slot<Descriptor>] =
        core::slice::from_raw_parts(desc_ptr as *const Slot<Descriptor>, cap);
    Ring::from_parts(slots, idx)
}

/// **Retire** the highest drained seqno on the shared ring so the guest's fences resolve. Kept
/// separate from [`pop_batch`] because it happens *after* phase 2 (descriptor execution), when the
/// caller re-views the ring. A no-op if `highest_seqno == 0` (nothing drained).
///
/// # Safety
/// Same contract as [`ring_over_shared`].
pub unsafe fn retire_over_shared(
    index_ptr: *const u8,
    desc_ptr: *const u8,
    cap: usize,
    highest_seqno: u64,
) -> Result<(), RingError> {
    if highest_seqno == 0 {
        return Ok(());
    }
    let ring = ring_over_shared(index_ptr, desc_ptr, cap)?;
    ring.retire(highest_seqno);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use infinigpu_abi::wire::{msg_type, Descriptor};

    /// An owned index page + descriptor slot array standing in for the sparse-mmap'd guest ring,
    /// so the drain logic is exercised with zero QEMU/vfio-user. `Indices` is `align(64)` and the
    /// slots are a contiguous array — the same shapes `ring_over_shared` views in production.
    struct OwnedRing {
        idx: Box<Indices>,
        slots: Vec<Slot<Descriptor>>,
    }

    fn zero_desc() -> Descriptor {
        Descriptor {
            msg_type: 0,
            flags: 0,
            len: 0,
            data_offset: 0,
            seqno: 0,
            _reserved: 0,
        }
    }

    impl OwnedRing {
        fn new(cap: usize) -> Self {
            let mut slots = Vec::with_capacity(cap);
            for _ in 0..cap {
                slots.push(Slot::new(zero_desc()));
            }
            OwnedRing { idx: Box::new(Indices::new()), slots }
        }
        fn ring(&self) -> Ring<'_, Descriptor> {
            Ring::from_parts(&self.slots, &self.idx).unwrap()
        }
    }

    fn desc(msg_type: u32, seqno: u64) -> Descriptor {
        Descriptor { msg_type, flags: 0, len: 0, data_offset: 0, seqno, _reserved: 0 }
    }

    #[test]
    fn drains_all_when_under_the_bound() {
        let r = OwnedRing::new(8);
        let ring = r.ring();
        for i in 1..=5u64 {
            ring.push(desc(msg_type::RESOURCE_FLUSH, i)).unwrap();
        }
        let drained = pop_batch(&ring, 8);
        assert_eq!(drained.descriptors.len(), 5);
        assert_eq!(drained.highest_seqno, 5);
        // FIFO order preserved.
        assert_eq!(drained.descriptors[0].seqno, 1);
        assert_eq!(drained.descriptors[4].seqno, 5);
        // The ring is now empty (head advanced to tail).
        assert!(ring.is_empty());
    }

    #[test]
    fn bounds_the_batch_and_leaves_the_rest_for_the_next_poll() {
        let r = OwnedRing::new(8);
        let ring = r.ring();
        for i in 1..=8u64 {
            ring.push(desc(msg_type::SUBMIT_CMD, i)).unwrap();
        }
        // A drain bounded at 3 takes only the first 3; the producer's work isn't lost.
        let first = pop_batch(&ring, 3);
        assert_eq!(first.descriptors.len(), 3);
        assert_eq!(first.highest_seqno, 3);
        assert_eq!(ring.len(), 5);
        // The next poll picks up the remainder (still bounded).
        let second = pop_batch(&ring, 3);
        assert_eq!(second.descriptors.len(), 3);
        assert_eq!(second.highest_seqno, 6);
        let third = pop_batch(&ring, 3);
        assert_eq!(third.descriptors.len(), 2);
        assert_eq!(third.highest_seqno, 8);
        assert!(ring.is_empty());
    }

    #[test]
    fn max_is_clamped_to_capacity_and_empty_ring_is_a_noop() {
        let r = OwnedRing::new(4);
        let ring = r.ring();
        // Empty ring → nothing drained, no seqno.
        let empty = pop_batch(&ring, 999);
        assert!(empty.descriptors.is_empty());
        assert_eq!(empty.highest_seqno, 0);
        // Fill it, then ask for more than capacity — clamped to what's there (never over-reads).
        for i in 1..=4u64 {
            ring.push(desc(msg_type::CURSOR_UPDATE, i)).unwrap();
        }
        let drained = pop_batch(&ring, usize::MAX);
        assert_eq!(drained.descriptors.len(), 4);
        assert_eq!(drained.highest_seqno, 4);
    }

    #[test]
    fn ring_over_shared_views_owned_memory_and_two_phase_drain_retires() {
        // Prove the from_ptr view + the full pop→(execute)→retire cycle over one shared page,
        // exactly as the device will (minus the vfio-user transport). This is the accept criterion
        // "push N descriptors + bump tail + one doorbell → head==tail, seqno_retired==N".
        const CAP: usize = 8;
        const N: u64 = 6;
        let mut r = OwnedRing::new(CAP);
        // Producer: publish N descriptors through the same Ring the guest would.
        {
            let ring = r.ring();
            for i in 1..=N {
                ring.push(desc(msg_type::RESOURCE_CREATE_BLOB, i)).unwrap();
            }
        }
        let index_ptr = (&*r.idx as *const Indices) as *const u8;
        let desc_ptr = r.slots.as_ptr() as *const u8;

        // Phase 1: view the shared page, pop a bounded batch, drop the view.
        let drained = {
            let ring = unsafe { ring_over_shared(index_ptr, desc_ptr, CAP) }.unwrap();
            pop_batch(&ring, CAP)
        };
        assert_eq!(drained.descriptors.len() as u64, N);
        assert_eq!(drained.highest_seqno, N);

        // Phase 2 would execute here (device-side); nothing to do in the unit.

        // Phase 3: retire over a fresh view — the guest observes seqno_retired == N and head == tail.
        unsafe { retire_over_shared(index_ptr, desc_ptr, CAP, drained.highest_seqno) }.unwrap();
        let ring = unsafe { ring_over_shared(index_ptr, desc_ptr, CAP) }.unwrap();
        assert_eq!(ring.retired(), N, "guest sees all N retired");
        assert!(ring.is_empty(), "head advanced to tail");
        // Silence unused-mut on r after the borrows.
        let _ = &mut r;
    }

    #[test]
    fn retire_zero_is_a_noop() {
        const CAP: usize = 4;
        let r = OwnedRing::new(CAP);
        let index_ptr = (&*r.idx as *const Indices) as *const u8;
        let desc_ptr = r.slots.as_ptr() as *const u8;
        unsafe { retire_over_shared(index_ptr, desc_ptr, CAP, 0) }.unwrap();
        let ring = unsafe { ring_over_shared(index_ptr, desc_ptr, CAP) }.unwrap();
        assert_eq!(ring.retired(), 0);
    }

    #[test]
    fn bad_capacity_is_rejected_fail_closed() {
        let r = OwnedRing::new(4);
        let index_ptr = (&*r.idx as *const Indices) as *const u8;
        let desc_ptr = r.slots.as_ptr() as *const u8;
        // Non-power-of-two capacity is rejected, not silently accepted.
        assert_eq!(
            unsafe { ring_over_shared(index_ptr, desc_ptr, 3) }.err(),
            Some(RingError::BadGeometry)
        );
    }
}

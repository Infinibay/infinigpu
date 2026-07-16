//! # infinigpu-ring
//!
//! A `no_std`, allocation-free **single-producer / single-consumer** descriptor
//! ring with seqno completion — the data-structure half of the wire protocol
//! (ADR-0004, research/11 §2). The guest is the producer (submits descriptors),
//! the host is the consumer (drains and retires them); a monotonic **retired
//! seqno** word publishes completion so fences resolve without a second ring.
//!
//! The ring is a *view* over caller-provided memory ([`Ring::from_parts`]): in
//! production those are the mmap'd shared index page + the descriptor array in
//! guest RAM. The crate itself allocates nothing.
//!
//! ## Memory ordering
//!
//! Producer owns `tail`/`seqno_submit`; consumer owns `head`/`seqno_retired`/
//! `status`. The publish protocol is the classic SPSC pair — producer writes the
//! slot then `Release`-stores `tail`; consumer `Acquire`-loads `tail` before
//! reading the slot (symmetrically for `head`). This establishes the
//! happens-before that makes the slot data race-free. The ordering is verified
//! under [`loom`](https://docs.rs/loom) (see `tests/loom_ring.rs`), per ADR-0004.

#![cfg_attr(not(test), no_std)]
#![allow(clippy::missing_safety_doc)]

use core::marker::PhantomData;

/// Atomics + interior-mutability cell, sourced from `loom` under `--cfg loom` and
/// from `core` otherwise, so the exact same ring code is model-checked and shipped.
mod sync {
    #[cfg(loom)]
    pub use loom::cell::UnsafeCell;
    #[cfg(loom)]
    pub use loom::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

    #[cfg(not(loom))]
    pub use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

    // A tiny shim giving `core::cell::UnsafeCell` the same `with`/`with_mut`
    // closure API loom uses, so ring code is identical in both builds.
    #[cfg(not(loom))]
    #[derive(Debug)]
    pub struct UnsafeCell<T>(core::cell::UnsafeCell<T>);

    #[cfg(not(loom))]
    impl<T> UnsafeCell<T> {
        #[inline]
        pub const fn new(v: T) -> Self {
            Self(core::cell::UnsafeCell::new(v))
        }
        #[inline]
        pub fn with<R>(&self, f: impl FnOnce(*const T) -> R) -> R {
            f(self.0.get())
        }
        #[inline]
        pub fn with_mut<R>(&self, f: impl FnOnce(*mut T) -> R) -> R {
            f(self.0.get())
        }
    }
}

use sync::{fence, AtomicU32, AtomicU64, Ordering, UnsafeCell};

/// One ring slot. `unsafe impl Sync` is sound because the SPSC protocol guarantees
/// the producer and consumer never touch the same slot concurrently — a slot is
/// written before `tail` is published and read before `head` frees it.
#[repr(transparent)]
pub struct Slot<T>(UnsafeCell<T>);

// SAFETY: SPSC discipline (below) serialises all access to a given slot through
// the tail/head Release/Acquire edges; no two threads ever alias one slot.
unsafe impl<T: Send> Sync for Slot<T> {}

#[cfg(not(loom))]
impl<T> Slot<T> {
    /// `const` so slot arrays can be statically initialised in the guest driver.
    #[inline]
    pub const fn new(v: T) -> Self {
        Slot(UnsafeCell::new(v))
    }
}

#[cfg(loom)]
impl<T> Slot<T> {
    #[inline]
    pub fn new(v: T) -> Self {
        Slot(UnsafeCell::new(v))
    }
}

/// The atomic index words — the runtime, in-memory view of
/// [`infinigpu_abi::wire::RingIndices`]. Field semantics and ownership match that
/// struct exactly; only the representation (atomics vs. plain ints) differs.
#[derive(Debug)]
pub struct Indices {
    tail: AtomicU32,
    head: AtomicU32,
    seqno_submit: AtomicU64,
    seqno_retired: AtomicU64,
    status: AtomicU32,
}

impl Indices {
    #[inline]
    pub fn new() -> Self {
        Self {
            tail: AtomicU32::new(0),
            head: AtomicU32::new(0),
            seqno_submit: AtomicU64::new(0),
            seqno_retired: AtomicU64::new(0),
            status: AtomicU32::new(0),
        }
    }
}

impl Default for Indices {
    fn default() -> Self {
        Self::new()
    }
}

/// Errors from ring operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingError {
    /// Capacity is not a non-zero power of two, or slots/capacity disagree.
    BadGeometry,
    /// The ring is full (producer) — retry after the consumer drains.
    Full,
}

/// An SPSC descriptor ring viewed over caller-provided `slots` + `indices`.
///
/// `capacity` (== `slots.len()`) must be a power of two. The same `Ring` value
/// exposes both ends; by the SPSC contract exactly one thread calls the producer
/// methods ([`push`](Ring::push)) and one calls the consumer methods
/// ([`pop`](Ring::pop)).
pub struct Ring<'a, T: Copy> {
    slots: &'a [Slot<T>],
    idx: &'a Indices,
    mask: u32,
    _pd: PhantomData<T>,
}

impl<'a, T: Copy> Ring<'a, T> {
    /// Build a ring view. `slots.len()` must be a non-zero power of two ≤ `u32::MAX`.
    pub fn from_parts(slots: &'a [Slot<T>], idx: &'a Indices) -> Result<Self, RingError> {
        let cap = slots.len();
        if cap == 0 || !cap.is_power_of_two() || cap > u32::MAX as usize {
            return Err(RingError::BadGeometry);
        }
        Ok(Ring {
            slots,
            idx,
            mask: (cap - 1) as u32,
            _pd: PhantomData,
        })
    }

    #[inline]
    pub fn capacity(&self) -> u32 {
        self.mask + 1
    }

    /// Number of unconsumed entries (observed; may be stale by the time it returns).
    #[inline]
    pub fn len(&self) -> u32 {
        let tail = self.idx.tail.load(Ordering::Acquire);
        let head = self.idx.head.load(Ordering::Acquire);
        tail.wrapping_sub(head)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    // ---- Producer side (guest) — owns `tail`, `seqno_submit` ----

    /// Publish one entry. Returns the assigned submission seqno on success, or
    /// [`RingError::Full`]. Producer-only.
    pub fn push(&self, item: T) -> Result<u64, RingError> {
        let tail = self.idx.tail.load(Ordering::Relaxed); // producer owns tail
        let head = self.idx.head.load(Ordering::Acquire); // observe freed slots
        if tail.wrapping_sub(head) >= self.capacity() {
            return Err(RingError::Full);
        }

        let slot = &self.slots[(tail & self.mask) as usize];
        // SAFETY: slot `tail & mask` is free (checked above) and not aliased by
        // the consumer, which cannot reach it until we publish `tail` below.
        slot.0.with_mut(|p| unsafe { p.write(item) });

        // seqno is 1-based and monotonic with the producer index.
        let seqno = tail.wrapping_add(1) as u64;
        self.idx.seqno_submit.store(seqno, Ordering::Relaxed);

        // Release: everything above (slot write) happens-before the consumer's
        // Acquire load of `tail`.
        self.idx.tail.store(tail.wrapping_add(1), Ordering::Release);
        Ok(seqno)
    }

    // ---- Consumer side (host) — owns `head`, `seqno_retired`, `status` ----

    /// Consume one entry, or `None` if empty. Consumer-only.
    pub fn pop(&self) -> Option<T> {
        let head = self.idx.head.load(Ordering::Relaxed); // consumer owns head
        let tail = self.idx.tail.load(Ordering::Acquire); // observe published slots
        if head == tail {
            return None;
        }

        let slot = &self.slots[(head & self.mask) as usize];
        // SAFETY: slot `head & mask` was published (Acquire on `tail` synchronises
        // with the producer's Release) and the producer will not reuse it until we
        // advance `head` below.
        let item = slot.0.with(|p| unsafe { p.read() });

        // Release: the slot read happens-before the producer sees the slot freed.
        self.idx.head.store(head.wrapping_add(1), Ordering::Release);
        Some(item)
    }

    /// Publish the highest retired seqno (completion). Consumer-only. Signalling
    /// the guest (MSI-X) is the transport's job; this only publishes the word.
    #[inline]
    pub fn retire(&self, seqno: u64) {
        self.idx.seqno_retired.store(seqno, Ordering::Release);
    }

    /// Observe the highest retired seqno. Any side; guest uses it to resolve fences.
    #[inline]
    pub fn retired(&self) -> u64 {
        self.idx.seqno_retired.load(Ordering::Acquire)
    }

    /// Highest submitted seqno (producer-published).
    #[inline]
    pub fn submitted(&self) -> u64 {
        self.idx.seqno_submit.load(Ordering::Acquire)
    }

    /// A submission with `out_fence == seqno` is done once `retired() >= seqno`
    /// (wrap-safe over the 64-bit space).
    #[inline]
    pub fn is_fence_signaled(&self, seqno: u64) -> bool {
        self.retired().wrapping_sub(seqno) < (1u64 << 63) && self.retired() >= seqno
    }

    /// Consumer sets per-ring status/error bits (`Release`).
    #[inline]
    pub fn set_status(&self, bits: u32) {
        self.idx.status.store(bits, Ordering::Release);
        fence(Ordering::Release);
    }

    /// Read per-ring status/error bits.
    #[inline]
    pub fn status(&self) -> u32 {
        self.idx.status.load(Ordering::Acquire)
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn make(cap: usize) -> (alloc_vec::Vec<Slot<u64>>, Indices) {
        let mut v = alloc_vec::Vec::with_capacity(cap);
        for _ in 0..cap {
            v.push(Slot::new(0u64));
        }
        (v, Indices::new())
    }

    // std is available under `cfg(test)`, so lean on it for the test harness only.
    mod alloc_vec {
        pub use std::vec::Vec;
    }

    #[test]
    fn rejects_non_power_of_two() {
        let (slots, idx) = make(3);
        assert_eq!(
            Ring::from_parts(&slots, &idx).err(),
            Some(RingError::BadGeometry)
        );
    }

    #[test]
    fn push_pop_fifo() {
        let (slots, idx) = make(4);
        let ring = Ring::from_parts(&slots, &idx).unwrap();
        assert!(ring.is_empty());
        for i in 0..4u64 {
            assert_eq!(ring.push(i * 10).unwrap(), i + 1);
        }
        // capacity 4 is now full
        assert_eq!(ring.push(999).err(), Some(RingError::Full));
        for i in 0..4u64 {
            assert_eq!(ring.pop(), Some(i * 10));
        }
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn wraps_around_indefinitely() {
        let (slots, idx) = make(2);
        let ring = Ring::from_parts(&slots, &idx).unwrap();
        // Push/pop far past capacity to exercise index wraparound.
        for i in 0..1000u64 {
            assert_eq!(ring.push(i).unwrap(), i + 1);
            assert_eq!(ring.pop(), Some(i));
        }
        assert!(ring.is_empty());
        assert_eq!(ring.submitted(), 1000);
    }

    #[test]
    fn fence_signaling_is_monotonic() {
        let (slots, idx) = make(2);
        let ring = Ring::from_parts(&slots, &idx).unwrap();
        assert!(!ring.is_fence_signaled(5));
        ring.retire(4);
        assert!(!ring.is_fence_signaled(5));
        ring.retire(5);
        assert!(ring.is_fence_signaled(5));
        assert!(ring.is_fence_signaled(4));
    }

    #[test]
    fn spsc_two_threads_lossless() {
        use std::sync::Arc;
        struct Storage {
            slots: Vec<Slot<u64>>,
            idx: Indices,
        }
        // SAFETY: Slot<u64>: Sync (SPSC), Indices is atomics → Storage is Sync.
        unsafe impl Sync for Storage {}

        const N: u64 = 100_000;
        let mut slots = Vec::new();
        for _ in 0..8 {
            slots.push(Slot::new(0u64));
        }
        let store = Arc::new(Storage {
            slots,
            idx: Indices::new(),
        });

        let prod = {
            let store = Arc::clone(&store);
            std::thread::spawn(move || {
                let ring = Ring::from_parts(&store.slots, &store.idx).unwrap();
                let mut i = 1u64;
                while i <= N {
                    if ring.push(i).is_ok() {
                        i += 1;
                    } else {
                        std::hint::spin_loop();
                    }
                }
            })
        };

        let ring = Ring::from_parts(&store.slots, &store.idx).unwrap();
        let mut expected = 1u64;
        while expected <= N {
            if let Some(v) = ring.pop() {
                assert_eq!(v, expected, "SPSC delivered out of order / lost an item");
                expected += 1;
            } else {
                std::hint::spin_loop();
            }
        }
        prod.join().unwrap();
        assert_eq!(expected, N + 1);
    }
}

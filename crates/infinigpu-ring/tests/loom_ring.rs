//! Exhaustive memory-ordering model check for the SPSC ring (ADR-0004: "a wrong
//! fence is a silent data race"). Compiled and run only under `--cfg loom`:
//!
//! ```sh
//! RUSTFLAGS="--cfg loom" cargo test -p infinigpu-ring --test loom_ring --release
//! ```
//!
//! loom explores every legal interleaving of the producer and consumer threads
//! and every permitted reordering of the relaxed/acquire/release atomics, and
//! also flags any concurrent aliasing of a ring slot's `UnsafeCell`. A green run
//! is a proof (over the bounded model) that the ring never loses, duplicates, or
//! reorders an entry and never races on slot memory.
#![cfg(loom)]

use infinigpu_ring::{Indices, Ring, Slot};
use loom::sync::Arc;

struct Storage {
    slots: Vec<Slot<u64>>,
    idx: Indices,
}
// SAFETY: Slot<u64>: Sync by the SPSC contract; Indices is atomics. Sharing the
// storage across the two ends is exactly what the model is checking.
unsafe impl Sync for Storage {}
unsafe impl Send for Storage {}

/// Capacity 2, three items — small enough for loom's exhaustive search yet large
/// enough to force full/empty wraparound and both Release/Acquire edges.
#[test]
fn spsc_ordering_is_lossless_and_race_free() {
    loom::model(|| {
        const CAP: usize = 2;
        const N: u64 = 3;

        let mut slots = Vec::with_capacity(CAP);
        for _ in 0..CAP {
            slots.push(Slot::new(0u64));
        }
        let store = Arc::new(Storage {
            slots,
            idx: Indices::new(),
        });

        let producer = {
            let store = Arc::clone(&store);
            loom::thread::spawn(move || {
                let ring = Ring::from_parts(&store.slots, &store.idx).unwrap();
                let mut i = 1u64;
                while i <= N {
                    match ring.push(i) {
                        Ok(seq) => {
                            assert_eq!(seq, i, "seqno must track the producer index");
                            i += 1;
                        }
                        Err(_) => loom::thread::yield_now(), // ring full; let consumer run
                    }
                }
            })
        };

        // Consumer runs on the model's main thread.
        let ring = Ring::from_parts(&store.slots, &store.idx).unwrap();
        let mut expected = 1u64;
        while expected <= N {
            match ring.pop() {
                Some(v) => {
                    assert_eq!(v, expected, "FIFO order / no loss / no duplication");
                    expected += 1;
                }
                None => loom::thread::yield_now(), // empty; let producer run
            }
        }

        producer.join().unwrap();
        assert_eq!(expected, N + 1);
    });
}

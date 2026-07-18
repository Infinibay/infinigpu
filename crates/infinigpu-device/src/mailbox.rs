//! Latest-wins mailbox — the 2D-ADR **PR5** seam (also its *biggest risk* mitigation).
//!
//! GPU convert/composite work must **never** run on the single vfio-user callback thread: parking
//! that thread across a GPU submit + broker throttle freezes the guest vCPU and QEMU's BQL, which
//! stalls the QMP monitor and freezes mouse/keyboard — the exact regression class fixed in
//! `f14ad69` (see the mouse-lag-hunt memory). The fix is to hand each present off to a **per-VM
//! worker thread** and let the callback thread return immediately.
//!
//! This is that hand-off: a single-slot, **latest-wins** channel. [`Sender::post`] never blocks
//! (safe on the callback thread) and *replaces* any unconsumed item — so a fast producer (page
//! flips) can't build an unbounded backlog for a slow consumer (GPU encode); the worker always
//! wakes to the freshest frame and stale intermediate frames are dropped, which is exactly right
//! for a display stream (bufferbloat is latency, not value — cf. the growing-lag/bufferbloat
//! memory). The worker blocks in [`Receiver::recv`] until work arrives or the mailbox closes.
//!
//! It is transport- and payload-agnostic (`T` is whatever the caller hands off — a present job),
//! so the coalescing + wakeup + shutdown semantics are fully unit-tested off-hardware, decoupled
//! from the GPU convert body (which needs the A5000) it will eventually drive.

use std::sync::{Arc, Condvar, Mutex};

struct Slot<T> {
    item: Option<T>,
    closed: bool,
}

struct Inner<T> {
    slot: Mutex<Slot<T>>,
    cv: Condvar,
}

/// The producer end (held by the callback thread). Cloneable — every clone posts to the same slot.
pub struct Sender<T> {
    inner: Arc<Inner<T>>,
}

/// The consumer end (held by the per-VM worker thread).
pub struct Receiver<T> {
    inner: Arc<Inner<T>>,
}

/// Create a latest-wins mailbox: `(Sender, Receiver)` over one shared slot.
pub fn mailbox<T>() -> (Sender<T>, Receiver<T>) {
    let inner = Arc::new(Inner {
        slot: Mutex::new(Slot { item: None, closed: false }),
        cv: Condvar::new(),
    });
    (Sender { inner: Arc::clone(&inner) }, Receiver { inner })
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Sender { inner: Arc::clone(&self.inner) }
    }
}

impl<T> Sender<T> {
    /// Post the latest work, **dropping** any unconsumed previous item (latest-wins coalescing) and
    /// returning it. Never blocks — safe to call on the vfio-user callback thread. Wakes the worker.
    /// A no-op returning `Some(item)` back to the caller if the mailbox is already closed (so the
    /// caller can reclaim it).
    pub fn post(&self, item: T) -> Option<T> {
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.closed {
            return Some(item);
        }
        let prev = slot.item.replace(item);
        drop(slot);
        // One waiter (the single per-VM worker); notify_one is enough and cheap.
        self.inner.cv.notify_one();
        prev
    }

    /// Signal the worker to stop: a blocked [`Receiver::recv`] returns `None`. Idempotent.
    pub fn close(&self) {
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.closed = true;
        drop(slot);
        self.inner.cv.notify_all();
    }
}

impl<T> Receiver<T> {
    /// Block until an item is posted or the mailbox closes. Returns the latest item, or `None` once
    /// the mailbox is closed **and** empty (the worker then exits its loop).
    pub fn recv(&self) -> Option<T> {
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(item) = slot.item.take() {
                return Some(item);
            }
            if slot.closed {
                return None;
            }
            slot = self.inner.cv.wait(slot).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Non-blocking take of the current item, if any. Never blocks; ignores closed-ness (drains any
    /// last item first).
    pub fn try_take(&self) -> Option<T> {
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.item.take()
    }

    /// Whether the mailbox has been closed by a sender.
    pub fn is_closed(&self) -> bool {
        self.inner.slot.lock().unwrap_or_else(|e| e.into_inner()).closed
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        // A gone worker means a `post` should stop pretending work will be consumed: mark closed so
        // the callback thread's `post` returns the item back instead of leaking it into a dead slot.
        let mut slot = self.inner.slot.lock().unwrap_or_else(|e| e.into_inner());
        slot.closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn latest_wins_coalesces_unconsumed_items() {
        let (tx, rx) = mailbox::<u32>();
        // Two posts with no intervening recv: the first is dropped and returned, the worker only
        // ever sees the freshest.
        assert_eq!(tx.post(1), None);
        assert_eq!(tx.post(2), Some(1), "the stale item is coalesced out and handed back");
        assert_eq!(rx.recv(), Some(2));
        // Slot is now empty.
        assert_eq!(rx.try_take(), None);
    }

    #[test]
    fn recv_blocks_until_a_post_arrives() {
        let (tx, rx) = mailbox::<u32>();
        let seen = Arc::new(AtomicU32::new(0));
        let seen2 = Arc::clone(&seen);
        let worker = std::thread::spawn(move || {
            // Blocks here until main posts.
            if let Some(v) = rx.recv() {
                seen2.store(v, Ordering::SeqCst);
            }
        });
        // Give the worker time to reach the blocking wait, then post.
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(seen.load(Ordering::SeqCst), 0, "worker must still be blocked with no work");
        tx.post(42);
        worker.join().unwrap();
        assert_eq!(seen.load(Ordering::SeqCst), 42);
    }

    #[test]
    fn close_wakes_a_blocked_receiver_with_none() {
        let (tx, rx) = mailbox::<u32>();
        let worker = std::thread::spawn(move || rx.recv());
        std::thread::sleep(Duration::from_millis(20));
        tx.close();
        assert_eq!(worker.join().unwrap(), None, "closed+empty recv returns None so the worker exits");
    }

    #[test]
    fn close_still_drains_a_pending_item_first() {
        let (tx, rx) = mailbox::<u32>();
        tx.post(7);
        tx.close();
        // A pending item is delivered before the close is observed (no work is lost on shutdown).
        assert_eq!(rx.recv(), Some(7));
        assert_eq!(rx.recv(), None);
    }

    #[test]
    fn post_after_close_hands_the_item_back() {
        let (tx, rx) = mailbox::<u32>();
        tx.close();
        assert_eq!(tx.post(9), Some(9), "posting into a closed mailbox returns the item to the caller");
        let _ = rx;
    }

    #[test]
    fn dropping_the_receiver_closes_the_mailbox() {
        let (tx, rx) = mailbox::<u32>();
        drop(rx);
        // The worker is gone: the callback thread's post reclaims its item instead of leaking it.
        assert_eq!(tx.post(5), Some(5));
    }

    #[test]
    fn a_fast_producer_never_backs_up_a_slow_consumer() {
        // Post many items while the consumer is "busy"; it must only ever pull the freshest, and
        // the slot never holds more than one — the anti-bufferbloat property.
        let (tx, rx) = mailbox::<u32>();
        for i in 1..=1000 {
            tx.post(i);
        }
        // Exactly one item is buffered (the latest); the other 999 were coalesced away.
        assert_eq!(rx.recv(), Some(1000));
        assert_eq!(rx.try_take(), None);
    }
}

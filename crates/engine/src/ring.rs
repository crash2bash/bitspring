//! Lock-free single-producer / single-consumer ring of f32 samples.
//!
//! The render thread (producer) fills it with processed audio; the CoreAudio IOProc
//! (consumer) drains it. Neither side ever blocks or allocates: each side only moves its own
//! counter, and reads the other's with Acquire to see the slots it published.

use std::cell::UnsafeCell;
use std::sync::atomic::{AtomicUsize, Ordering};

pub struct Ring {
    slots: Box<[UnsafeCell<f32>]>,
    /// Total samples ever written / read (monotonic; index = counter % capacity).
    written: AtomicUsize,
    read: AtomicUsize,
}

// SAFETY: a slot is written only by the producer while it is outside [read, written), and read
// only by the consumer while inside it; the Release/Acquire pairs on the counters order those
// accesses. Callers uphold "one producer thread, one consumer thread" (see Player).
unsafe impl Sync for Ring {}
unsafe impl Send for Ring {}

impl Ring {
    pub fn new(capacity: usize) -> Self {
        Self {
            slots: (0..capacity).map(|_| UnsafeCell::new(0.0)).collect(),
            written: AtomicUsize::new(0),
            read: AtomicUsize::new(0),
        }
    }

    pub fn capacity(&self) -> usize {
        self.slots.len()
    }

    /// Samples ready for the consumer. Safe to call from any thread: `read` is loaded first,
    /// so a push and pop landing between the two loads can't make it exceed `written` (an
    /// underflow, and in debug builds a panic under the player lock). For a third-thread
    /// caller the answer is only a snapshot, clamped to what the ring can hold.
    pub fn available(&self) -> usize {
        let r = self.read.load(Ordering::Acquire);
        let w = self.written.load(Ordering::Acquire);
        w.saturating_sub(r).min(self.capacity())
    }

    /// Room for the producer.
    pub fn free(&self) -> usize {
        self.capacity() - self.available()
    }

    /// Producer only. Writes as much of `data` as fits; returns how many samples.
    pub fn push(&self, data: &[f32]) -> usize {
        let w = self.written.load(Ordering::Relaxed);
        let r = self.read.load(Ordering::Acquire);
        let n = data.len().min(self.capacity() - (w - r));
        for (i, &s) in data[..n].iter().enumerate() {
            // SAFETY: slot (w+i) is free — the consumer has read past it (see type docs).
            unsafe { *self.slots[(w + i) % self.capacity()].get() = s };
        }
        self.written.store(w + n, Ordering::Release);
        n
    }

    /// Consumer only. Fills `out` from the ring; returns how many samples.
    pub fn pop(&self, out: &mut [f32]) -> usize {
        let r = self.read.load(Ordering::Relaxed);
        let w = self.written.load(Ordering::Acquire);
        let n = out.len().min(w - r);
        for (i, o) in out[..n].iter_mut().enumerate() {
            // SAFETY: slot (r+i) was published by the producer's Release store.
            *o = unsafe { *self.slots[(r + i) % self.capacity()].get() };
        }
        self.read.store(r + n, Ordering::Release);
        n
    }

    /// Consumer only. Drop everything currently queued (used after a seek).
    pub fn discard(&self) {
        let w = self.written.load(Ordering::Acquire);
        self.read.store(w, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn wraps_and_preserves_order_across_threads() {
        let ring = Arc::new(Ring::new(1000));
        let producer = {
            let ring = ring.clone();
            std::thread::spawn(move || {
                let data: Vec<f32> = (0..200_000).map(|i| i as f32).collect();
                let mut sent = 0;
                while sent < data.len() {
                    sent += ring.push(&data[sent..(sent + 333).min(data.len())]);
                    std::hint::spin_loop();
                }
            })
        };
        let mut got = Vec::with_capacity(200_000);
        let mut buf = [0f32; 257];
        while got.len() < 200_000 {
            let n = ring.pop(&mut buf);
            got.extend_from_slice(&buf[..n]);
        }
        producer.join().unwrap();
        assert!(got.iter().enumerate().all(|(i, &v)| v == i as f32));
    }
}

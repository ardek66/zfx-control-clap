//! Host-side publish→done wait sampling (off the audio critical path).
//!
//! ## How to read these stats
//!
//! - `hits` / `misses`: same-callback `wait_done` outcomes. A miss escalates to
//!   one `live_block` of PDC spill; hits stay in-callback.
//! - Ring samples carry `(live_block, os_factor, wait_us, hit)`. Compare small
//!   device sizes (64/128) vs large (1024–2048): low miss rates at large blocks
//!   mean the wait-fill curve still protects mix chains; rising miss rate or
//!   `wait_us` at 64/128 points at scheduling/IPC wake cost rather than DSP work.
//! - Drain / log from the main thread (`on_main_thread` / `request_callback`),
//!   never from the audio thread. Audio only does atomics + a ring store.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Power-of-two sample capacity. Audio overwrites; main drains when dirty.
const RING_CAP: usize = 256;
const RING_MASK: u32 = (RING_CAP as u32) - 1;

/// One publish→done observation. Written from the audio thread, read off-RT.
#[derive(Clone, Copy, Default)]
pub struct IpcWaitSample {
    pub live_block: u32,
    pub os_factor: u8,
    pub hit: u8,
    pub wait_us: u32,
}

/// Lock-free SPSC ring + aggregate counters for IPC wait latency.
pub struct IpcTimingStats {
    write: AtomicU32,
    read: AtomicU32,
    /// Slots are only touched by the single audio writer or the main-thread
    /// drain after a sample index is claimed; atomics gate ownership.
    slots: [std::cell::UnsafeCell<IpcWaitSample>; RING_CAP],
    pub hits: AtomicU64,
    pub misses: AtomicU64,
    pub sum_wait_us: AtomicU64,
    /// Set when new samples are available; cleared by [`Self::drain_log`].
    pub dirty: AtomicBool,
    /// Counts records since last log request (throttle `request_callback`).
    since_log: AtomicU32,
}

/// Ask the host main thread to drain after this many audio-thread samples.
const LOG_EVERY_N: u32 = 256;

// SAFETY: single audio writer, single main-thread reader; indices fence ownership.
unsafe impl Sync for IpcTimingStats {}
// SAFETY: same as Sync — ownership is sequenced by atomics.
unsafe impl Send for IpcTimingStats {}

impl IpcTimingStats {
    #[must_use]
    pub fn new() -> Self {
        Self {
            write: AtomicU32::new(0),
            read: AtomicU32::new(0),
            slots: std::array::from_fn(|_| {
                std::cell::UnsafeCell::new(IpcWaitSample::default())
            }),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            sum_wait_us: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            since_log: AtomicU32::new(0),
        }
    }

    /// Audio-thread path: atomics + one ring store. No allocation, no locks.
    ///
    /// Returns `true` every [`LOG_EVERY_N`] samples so the caller can
    /// `request_callback` without waking the host every block.
    #[inline]
    pub fn record(&self, live_block: u32, os_factor: u8, wait_us: u32, hit: bool) -> bool {
        if hit {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        self.sum_wait_us
            .fetch_add(u64::from(wait_us), Ordering::Relaxed);

        let idx = self.write.fetch_add(1, Ordering::Relaxed);
        let slot = usize::try_from(idx & RING_MASK).unwrap_or(0);
        // SAFETY: audio thread is the sole writer; each index maps to one slot
        // and main only reads after `dirty` / drained indices.
        unsafe {
            *self.slots[slot].get() = IpcWaitSample {
                live_block,
                os_factor,
                hit: u8::from(hit),
                wait_us,
            };
        }
        // Advance read if we wrapped (drop oldest unread).
        let write = idx.wrapping_add(1);
        let read = self.read.load(Ordering::Relaxed);
        if write.wrapping_sub(read) > RING_CAP as u32 {
            self.read
                .store(write.wrapping_sub(RING_CAP as u32), Ordering::Relaxed);
        }
        self.dirty.store(true, Ordering::Release);
        let n = self.since_log.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if n >= LOG_EVERY_N {
            self.since_log.store(0, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    /// Off-RT: consume ring samples and log a one-line summary.
    pub fn drain_log(&self) {
        if !self.dirty.swap(false, Ordering::AcqRel) {
            return;
        }
        let hits = self.hits.load(Ordering::Relaxed);
        let misses = self.misses.load(Ordering::Relaxed);
        let sum = self.sum_wait_us.load(Ordering::Relaxed);
        let total = hits.saturating_add(misses);
        let mean_us = sum.checked_div(total).unwrap_or(0);

        let mut n = 0u32;
        let mut last = IpcWaitSample::default();
        let write = self.write.load(Ordering::Acquire);
        loop {
            let read = self.read.load(Ordering::Relaxed);
            if read == write {
                break;
            }
            let slot = usize::try_from(read & RING_MASK).unwrap_or(0);
            // SAFETY: this index was published by `record` and has not been
            // reclaimed; only this drain path advances `read`.
            last = unsafe { *self.slots[slot].get() };
            self.read.store(read.wrapping_add(1), Ordering::Release);
            n = n.saturating_add(1);
        }

        eprintln!(
            "[zfx ipc] hits={hits} misses={misses} mean_wait_us={mean_us} \
             drained={n} last: live_block={} os={} wait_us={} hit={}",
            last.live_block, last.os_factor, last.wait_us, last.hit
        );
    }
}

impl Default for IpcTimingStats {
    fn default() -> Self {
        Self::new()
    }
}

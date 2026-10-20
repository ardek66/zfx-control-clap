//! Shared-memory IPC layout and futex helpers for the ZFX CLAP bridge.
//!
//! Both the 64-bit host (`zfx-bootstrap`) and the 32-bit worker (`zfx-worker32`)
//! depend on this crate so the `ZfxSharedBridge` ABI cannot drift.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const BRIDGE_MAGIC: u32 = 0x5A46_5842; // 'ZFXB'
pub const MAX_FRAMES: usize = 16384;
pub const MAX_PARAMS: usize = 128;
pub const MAX_PARAM_QUEUE: usize = 64;
pub const MAX_PAYLOAD_SIZE: usize = 512 * 1024; // 512 KB

pub const CMD_IDLE: u32 = 0;
pub const CMD_INIT: u32 = 1;
pub const CMD_SHUTDOWN: u32 = 3;

/// Audio block. The host waits in the same callback and plays this block's output.
pub const SLOT_AUDIO: u8 = 0;
/// Parameter update with no samples. The host does not play it back.
pub const SLOT_PARAMS: u8 = 1;

/// Depth-2 queue: one slot may still be finishing after a miss while the host
/// publishes the next block. When spilled, the host plays the prior finished
/// block and reports one live block via `clap.latency`.
pub const SLOT_COUNT: usize = 2;

pub const STATUS_READY: u32 = 0;
pub const STATUS_BUSY: u32 = 1;
pub const STATUS_OK: u32 = 2;
pub const STATUS_ERROR: u32 = 3;

#[repr(C)]
#[derive(Clone, Copy, Default, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct ParamChange {
    pub id: u32,
    pub value: f32,
}

#[repr(C)]
#[derive(Clone, Copy, FromBytes, IntoBytes, KnownLayout, Immutable)]
pub struct ParamMeta {
    pub id: u32,
    pub name: [u8; 32],
    pub default_val: f32,
}

impl Default for ParamMeta {
    fn default() -> Self {
        Self {
            id: 0,
            name: [0u8; 32],
            default_val: 0.0,
        }
    }
}

/// One audio block plus the parameter and transport changes that belong to it.
///
/// The sample planes are written in place. Indices are `AtomicU32` so the
/// 32-bit worker and the 64-bit host agree on the header width.
#[repr(C, align(8))]
pub struct ProcessSlot {
    pub num_frames: u32,
    pub num_param_changes: u32,
    pub sample_rate: f32,
    pub block_size: u32,
    pub host_frames: u32,
    pub time_sig_num: u16,
    pub time_sig_denom: u16,
    pub os_factor: u8,
    pub is_playing: u8,
    pub has_tempo: u8,
    pub kind: u8,
    pub pad: [u8; 4],
    pub tempo: f64,
    pub beat_pos: f64,
    pub param_queue: [ParamChange; MAX_PARAM_QUEUE],
    pub in_l: [f32; MAX_FRAMES],
    pub in_r: [f32; MAX_FRAMES],
    pub out_l: [f32; MAX_FRAMES],
    pub out_r: [f32; MAX_FRAMES],
}

/// Description of a finished audio block the host just copied out.
#[derive(Clone, Copy, Debug)]
pub struct CollectedBlock {
    pub host_frames: u32,
    pub dsp_frames: u32,
    pub os_factor: u8,
}

/// Shared memory layout exchanged between 64-bit CLAP host and 32-bit DSP worker.
///
/// Control commands (`CMD_INIT`, `CMD_SHUTDOWN`) use `host_seq` / `worker_seq`.
/// Audio uses `submit_seq` / `done_seq` and [`slots`](Self::slots). The host
/// publishes a block and waits in-callback up to a curve budget; on miss it
/// plays a prior finished block and reports one live block via `clap.latency`.
#[repr(C, align(8))]
pub struct ZfxSharedBridge {
    pub magic: u32,
    pub host_seq: AtomicU32,
    pub worker_seq: AtomicU32,
    pub command: AtomicU32,
    pub status: AtomicU32,
    /// Blocks the host has published. Only the host writes this.
    pub submit_seq: AtomicU32,
    /// Blocks the worker has finished. Only the worker writes this.
    pub done_seq: AtomicU32,
    /// Bumped when the host has published a command or a block.
    pub doorbell: AtomicU32,

    pub sample_rate: f32,
    pub block_size: u32,
    pub num_params: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub pad: u32,

    pub params: [ParamMeta; MAX_PARAMS],
    pub slots: [ProcessSlot; SLOT_COUNT],

    /// Embedded DLL payload transferred on init.
    pub payload_size: u32,
    pub payload_bytes: [u8; MAX_PAYLOAD_SIZE],
}

impl ZfxSharedBridge {
    pub fn init_header(&mut self) {
        self.magic = BRIDGE_MAGIC;
        self.host_seq.store(0, Ordering::Relaxed);
        self.worker_seq.store(0, Ordering::Relaxed);
        self.command.store(CMD_IDLE, Ordering::Relaxed);
        self.status.store(STATUS_READY, Ordering::Relaxed);
        self.submit_seq.store(0, Ordering::Relaxed);
        self.done_seq.store(0, Ordering::Relaxed);
        self.doorbell.store(0, Ordering::Relaxed);
        self.sample_rate = 44100.0;
        self.block_size = 256;
        self.num_params = 0;
        self.num_inputs = 2;
        self.num_outputs = 2;
        self.pad = 0;
        self.payload_size = 0;
    }

    /// Wake the worker. The audio thread may call this. It does not wait.
    pub fn ring_doorbell(&self) {
        self.doorbell.fetch_add(1, Ordering::Release);
        // SAFETY: `doorbell` is an aligned AtomicU32 inside this bridge allocation.
        unsafe { futex_wake(&raw const self.doorbell) }
    }

    /// Publish one block into the free slot.
    ///
    /// `collected` is the host's private cursor of blocks it has already copied
    /// out. A slot still holding unread audio is not reused. Returns `false`
    /// when both slots are still occupied.
    pub fn try_publish(&self, collected: u32, fill: impl FnOnce(&mut ProcessSlot)) -> bool {
        let submit = self.submit_seq.load(Ordering::Relaxed);
        let done = self.done_seq.load(Ordering::Acquire);
        if submit.wrapping_sub(collected) >= SLOT_LIMIT || submit.wrapping_sub(done) >= SLOT_LIMIT {
            return false;
        }
        let index = slot_index(submit);
        // SAFETY: the host is the only publisher. This slot's previous block
        // has been finished by the worker (`submit - done < SLOT_COUNT`) and
        // copied out by the host (`submit - collected < SLOT_COUNT`).
        unsafe {
            let slot = &mut *self.slots.as_ptr().cast_mut().add(index);
            fill(slot);
        }
        self.submit_seq
            .store(submit.wrapping_add(1), Ordering::Release);
        self.ring_doorbell();
        true
    }

    /// Claim the oldest published block. Dropping the guard marks it finished.
    pub fn claim_block(&self) -> Option<ClaimedBlock<'_>> {
        let done = self.done_seq.load(Ordering::Relaxed);
        let submit = self.submit_seq.load(Ordering::Acquire);
        if done == submit {
            return None;
        }
        Some(ClaimedBlock {
            bridge: self,
            seq: done,
        })
    }

    /// Copy the oldest finished audio block into `left` / `right`.
    ///
    /// Parameter-only slots are discarded. Returns `None` when the worker has
    /// not finished an audio block yet.
    pub fn collect_output(
        &self,
        collected: &mut u32,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Option<CollectedBlock> {
        let done = self.done_seq.load(Ordering::Acquire);
        while done.wrapping_sub(*collected) > 0 {
            if let Some(info) = self.take_finished(collected, left, right, true) {
                return Some(info);
            }
        }
        None
    }

    /// Play the audio for `target` (`done_seq` value after that block finished).
    ///
    /// Older finished blocks — including late finishes from a previous miss —
    /// are discarded so the host never introduces a one-block delay.
    pub fn collect_target(
        &self,
        collected: &mut u32,
        target: u32,
        left: &mut [f32],
        right: &mut [f32],
    ) -> Option<CollectedBlock> {
        if !seq_reached(self.done_seq.load(Ordering::Acquire), target) {
            return None;
        }
        let mut played = None;
        while target.wrapping_sub(*collected) > 0 {
            let keep = collected.wrapping_add(1) == target;
            if let Some(info) = self.take_finished(collected, left, right, keep) {
                played = Some(info);
            }
        }
        played
    }

    /// Advance `collected` over one finished slot. When `keep` is set and the
    /// slot is audio, copy it into `left` / `right`.
    fn take_finished(
        &self,
        collected: &mut u32,
        left: &mut [f32],
        right: &mut [f32],
        keep: bool,
    ) -> Option<CollectedBlock> {
        let index = slot_index(*collected);
        // SAFETY: the host is the only collector. The worker has stored
        // `done_seq` past this block and will not write it again until the
        // host publishes over it, which happens only after this cursor moves.
        let slot = unsafe { &*self.slots.as_ptr().add(index) };
        let kind = slot.kind;
        let info = CollectedBlock {
            host_frames: slot.host_frames,
            dsp_frames: slot.num_frames,
            os_factor: slot.os_factor,
        };
        *collected = collected.wrapping_add(1);
        if kind == SLOT_PARAMS {
            return None;
        }
        if keep {
            let dsp_frames = usize::try_from(info.dsp_frames)
                .unwrap_or(0)
                .min(MAX_FRAMES);
            let copy_n = dsp_frames.min(left.len()).min(right.len());
            copy_clamped(&slot.out_l[..copy_n], &mut left[..copy_n]);
            copy_clamped(&slot.out_r[..copy_n], &mut right[..copy_n]);
            Some(info)
        } else {
            None
        }
    }

    /// Wait until the worker has finished every block up through `target`.
    ///
    /// Sleeps on `done_seq` until the target is reached or `timeout_us` of
    /// wall time is spent. Intermediate completions (e.g. a late block from a
    /// previous miss) re-enter the wait with the remaining budget so a chain
    /// does not spuriously miss the block just published. Returns `false` on
    /// timeout so a stuck DLL cannot hold the audio callback.
    pub fn wait_done(&self, target: u32, timeout_us: u64) -> bool {
        if timeout_us == 0 {
            return seq_reached(self.done_seq.load(Ordering::Acquire), target);
        }
        let deadline = Instant::now() + Duration::from_micros(timeout_us);
        loop {
            let done = self.done_seq.load(Ordering::Acquire);
            if seq_reached(done, target) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining_us = u64::try_from(deadline.saturating_duration_since(now).as_micros())
                .unwrap_or(0)
                .max(1);
            // SAFETY: `done_seq` is an aligned atomic in this bridge allocation.
            unsafe { futex_wait(&raw const self.done_seq, done, Some(remaining_us)) }
        }
    }

    /// Drop finished blocks without copying them. Used while bypassed.
    pub fn skip_finished(&self, collected: &mut u32) {
        let done = self.done_seq.load(Ordering::Acquire);
        if done.wrapping_sub(*collected) > 0 {
            *collected = done;
        }
    }
}

/// Worker-side claim of one published block.
pub struct ClaimedBlock<'a> {
    bridge: &'a ZfxSharedBridge,
    seq: u32,
}

impl ClaimedBlock<'_> {
    pub fn slot(&mut self) -> &mut ProcessSlot {
        let index = slot_index(self.seq);
        // SAFETY: this sequence number is the worker's exclusive claim. The
        // host does not publish into the slot until `done_seq` moves past it,
        // which this guard does on drop.
        unsafe { &mut *self.bridge.slots.as_ptr().cast_mut().add(index) }
    }
}

impl Drop for ClaimedBlock<'_> {
    fn drop(&mut self) {
        self.bridge
            .done_seq
            .store(self.seq.wrapping_add(1), Ordering::Release);
        // SAFETY: `done_seq` is the bridge word the host waits on.
        unsafe { futex_wake(&raw const self.bridge.done_seq) }
    }
}

/// `done` has reached `target` on a wrapping counter.
fn seq_reached(done: u32, target: u32) -> bool {
    done.wrapping_sub(target) < (1_u32 << 31)
}

const SLOT_LIMIT: u32 = 2;
const _: () = assert!(SLOT_COUNT == 2);

fn slot_index(seq: u32) -> usize {
    usize::try_from(seq & 1).unwrap_or(0)
}

fn copy_clamped(src: &[f32], dst: &mut [f32]) {
    for (out, &sample) in dst.iter_mut().zip(src) {
        *out = if sample.is_finite() {
            sample.clamp(-4.0, 4.0)
        } else {
            0.0
        };
    }
}

/// # Safety
///
/// `val_addr` must be non-null and point to a valid, aligned `AtomicU32` in shared memory.
#[inline]
pub unsafe fn futex_wait(val_addr: *const AtomicU32, expected: u32, timeout_us: Option<u64>) {
    #[cfg(target_os = "linux")]
    {
        let ts = timeout_us.and_then(|us| {
            let sec = libc::time_t::try_from(us / 1_000_000).ok()?;
            let nsec = libc::c_long::try_from((us % 1_000_000) * 1_000).ok()?;
            Some(libc::timespec {
                tv_sec: sec,
                tv_nsec: nsec,
            })
        });
        let ts_ptr = ts
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref::<libc::timespec>);
        // SAFETY: caller guarantees `val_addr` is a live AtomicU32 in shared memory.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                val_addr,
                libc::FUTEX_WAIT,
                expected,
                ts_ptr,
                std::ptr::null::<u32>(),
                0,
            );
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Threading::WaitOnAddress;
        let wait_ms = timeout_us.map_or(u32::MAX, |us| {
            u32::try_from((us / 1_000).max(1)).unwrap_or(u32::MAX)
        });
        let undisaligned_expected = expected;
        // SAFETY: caller guarantees `val_addr` is a live AtomicU32 in shared memory.
        unsafe {
            WaitOnAddress(
                val_addr.cast(),
                std::ptr::from_ref(&undisaligned_expected).cast(),
                4,
                wait_ms,
            );
        }
    }
}

/// # Safety
///
/// `val_addr` must be non-null and point to a valid, aligned `AtomicU32` in shared memory.
#[inline]
pub unsafe fn futex_wake(val_addr: *const AtomicU32) {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: caller guarantees `val_addr` is a live AtomicU32 in shared memory.
        unsafe {
            libc::syscall(
                libc::SYS_futex,
                val_addr,
                libc::FUTEX_WAKE,
                1, // wake 1 waiter
                std::ptr::null::<libc::timespec>(),
                std::ptr::null::<u32>(),
                0,
            );
        }
    }
    #[cfg(target_os = "windows")]
    {
        use windows_sys::Win32::System::Threading::WakeByAddressSingle;
        // SAFETY: caller guarantees `val_addr` is a live AtomicU32 in shared memory.
        unsafe {
            WakeByAddressSingle(val_addr.cast());
        }
    }
}

const _: () = assert!(std::mem::size_of::<ParamChange>() == 8);
const _: () = assert!(std::mem::align_of::<ParamChange>() == 4);
const _: () = assert!(std::mem::size_of::<ParamMeta>() == 40);
const _: () = assert!(std::mem::align_of::<ParamMeta>() == 4);
const _: () = assert!(std::mem::offset_of!(ProcessSlot, tempo) == 32);
const _: () = assert!(std::mem::offset_of!(ProcessSlot, beat_pos) == 40);
const _: () = assert!(std::mem::offset_of!(ProcessSlot, param_queue) == 48);
const _: () = assert!(std::mem::offset_of!(ProcessSlot, in_l) == 48 + 8 * MAX_PARAM_QUEUE);
const _: () = assert!(std::mem::size_of::<ProcessSlot>() == 262_704);
const _: () = assert!(std::mem::align_of::<ProcessSlot>() == 8);
const _: () = assert!(std::mem::size_of::<ZfxSharedBridge>() == 1_054_880);
const _: () = assert!(std::mem::align_of::<ZfxSharedBridge>() == 8);
const _: () = assert!(std::mem::offset_of!(ZfxSharedBridge, params) == 56);
const _: () = assert!(std::mem::offset_of!(ZfxSharedBridge, slots) == 5_176);
const _: () = assert!(std::mem::offset_of!(ZfxSharedBridge, payload_bytes) == 530_588);
const _: () = assert!(std::mem::align_of::<AtomicU32>() == 4);
const _: () = assert!(std::mem::size_of::<AtomicU32>() == 4);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_pod_sizes() {
        assert_eq!(std::mem::size_of::<ParamChange>(), 8);
        assert_eq!(std::mem::size_of::<ParamMeta>(), 40);
    }

    #[test]
    fn depth_two_does_not_overwrite_an_unread_block() {
        let bridge = aligned_bridge();
        let mut collected = 0u32;

        assert!(bridge.try_publish(collected, |slot| {
            slot.kind = SLOT_AUDIO;
            slot.num_frames = 2;
            slot.host_frames = 2;
            slot.in_l[0] = 1.0;
        }));
        assert!(bridge.try_publish(collected, |slot| {
            slot.kind = SLOT_AUDIO;
            slot.num_frames = 1;
            slot.host_frames = 1;
            slot.in_l[0] = 2.0;
        }));
        assert!(!bridge.try_publish(collected, |_| {}));

        {
            let mut claimed = bridge.claim_block().expect("first block");
            assert_eq!(claimed.slot().in_l[0], 1.0);
            claimed.slot().out_l[0] = 0.5;
            claimed.slot().out_r[0] = -0.25;
            claimed.slot().out_l[1] = 10.0;
            claimed.slot().out_r[1] = f32::NAN;
        }

        let mut left = [0.0f32; 4];
        let mut right = [0.0f32; 4];
        let got = bridge
            .collect_output(&mut collected, &mut left, &mut right)
            .expect("finished block");
        assert_eq!(got.dsp_frames, 2);
        assert_eq!(left[0], 0.5);
        assert_eq!(right[0], -0.25);
        assert_eq!(left[1], 4.0);
        assert_eq!(right[1], 0.0);

        assert!(bridge.try_publish(collected, |slot| {
            slot.kind = SLOT_AUDIO;
            slot.num_frames = 1;
            slot.host_frames = 1;
            slot.in_l[0] = 3.0;
        }));
        let mut second = bridge.claim_block().expect("second block");
        assert_eq!(second.slot().in_l[0], 2.0);
    }

    #[test]
    fn param_slots_are_not_played_back() {
        let bridge = aligned_bridge();
        let mut collected = 0u32;
        assert!(bridge.try_publish(collected, |slot| {
            slot.kind = SLOT_PARAMS;
            slot.num_frames = 0;
        }));
        {
            let _claimed = bridge.claim_block().expect("params");
        }
        let mut left = [0.0f32; 1];
        let mut right = [0.0f32; 1];
        assert!(bridge
            .collect_output(&mut collected, &mut left, &mut right)
            .is_none());
        assert_eq!(collected, 1);
    }

    #[test]
    fn collect_target_discards_older_audio() {
        let bridge = aligned_bridge();
        let mut collected = 0u32;

        assert!(bridge.try_publish(collected, |slot| {
            slot.kind = SLOT_AUDIO;
            slot.num_frames = 1;
            slot.host_frames = 1;
            slot.out_l[0] = 1.0;
            slot.out_r[0] = 1.0;
            slot.in_l[0] = 1.0;
        }));
        {
            let mut claimed = bridge.claim_block().expect("first");
            claimed.slot().out_l[0] = 1.0;
            claimed.slot().out_r[0] = 1.0;
        }

        assert!(bridge.try_publish(collected, |slot| {
            slot.kind = SLOT_AUDIO;
            slot.num_frames = 1;
            slot.host_frames = 1;
            slot.in_l[0] = 2.0;
        }));
        {
            let mut claimed = bridge.claim_block().expect("second");
            claimed.slot().out_l[0] = 2.0;
            claimed.slot().out_r[0] = 2.0;
        }

        let target = bridge.submit_seq.load(Ordering::Acquire);
        let mut left = [0.0f32; 1];
        let mut right = [0.0f32; 1];
        let got = bridge
            .collect_target(&mut collected, target, &mut left, &mut right)
            .expect("target audio");
        assert_eq!(got.dsp_frames, 1);
        assert_eq!(left[0], 2.0);
        assert_eq!(right[0], 2.0);
        assert_eq!(collected, target);
    }

    fn aligned_bridge() -> Box<ZfxSharedBridge> {
        // SAFETY: the bridge is a `repr(C)` bag of integers, floats, and
        // atomics, so an all-zero bit pattern is a valid value. `new_zeroed`
        // allocates with the struct alignment instead of building the 1 MiB
        // value on the stack.
        let mut bridge = unsafe { Box::<ZfxSharedBridge>::new_zeroed().assume_init() };
        bridge.init_header();
        bridge
    }
}

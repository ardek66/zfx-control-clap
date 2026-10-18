//! Shared-memory IPC layout and futex helpers for the ZFX CLAP bridge.
//!
//! Both the 64-bit host (`zfx-bootstrap`) and the 32-bit worker (`zfx-worker32`)
//! depend on this crate so the `ZfxSharedBridge` ABI cannot drift.

use std::sync::atomic::AtomicU32;

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

pub const BRIDGE_MAGIC: u32 = 0x5A46_5842; // 'ZFXB'
pub const MAX_FRAMES: usize = 16384;
pub const MAX_PARAMS: usize = 128;
pub const MAX_PARAM_QUEUE: usize = 64;
pub const MAX_PAYLOAD_SIZE: usize = 512 * 1024; // 512 KB

pub const CMD_IDLE: u32 = 0;
pub const CMD_INIT: u32 = 1;
pub const CMD_PROCESS: u32 = 2;
pub const CMD_SHUTDOWN: u32 = 3;

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

/// Shared memory layout exchanged between 64-bit CLAP host and 32-bit DSP worker.
#[repr(C)]
pub struct ZfxSharedBridge {
    pub magic: u32,
    pub host_seq: AtomicU32,
    pub worker_seq: AtomicU32,
    pub command: AtomicU32,
    pub status: AtomicU32,

    pub sample_rate: f32,
    pub block_size: u32,
    pub num_frames: u32,

    pub tempo: f64,
    pub beat_pos: f64,
    pub time_sig_num: u16,
    pub time_sig_denom: u16,
    pub is_playing: u8,
    pub has_tempo: u8,
    pub pad: [u8; 2],

    pub num_params: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,

    pub params: [ParamMeta; MAX_PARAMS],

    pub num_param_changes: u32,
    pub param_queue: [ParamChange; MAX_PARAM_QUEUE],

    pub in_l: [f32; MAX_FRAMES],
    pub in_r: [f32; MAX_FRAMES],
    pub out_l: [f32; MAX_FRAMES],
    pub out_r: [f32; MAX_FRAMES],

    /// Embedded DLL payload transferred on init.
    pub payload_size: u32,
    pub payload_bytes: [u8; MAX_PAYLOAD_SIZE],
}

impl ZfxSharedBridge {
    pub fn init_header(&mut self) {
        self.magic = BRIDGE_MAGIC;
        self.host_seq.store(0, std::sync::atomic::Ordering::Relaxed);
        self.worker_seq
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.command
            .store(CMD_IDLE, std::sync::atomic::Ordering::Relaxed);
        self.status
            .store(STATUS_READY, std::sync::atomic::Ordering::Relaxed);
        self.sample_rate = 44100.0;
        self.block_size = 256;
        self.num_frames = 0;
        self.tempo = 120.0;
        self.beat_pos = 0.0;
        self.time_sig_num = 4;
        self.time_sig_denom = 4;
        self.is_playing = 0;
        self.has_tempo = 1;
        self.pad = [0; 2];
        self.num_params = 0;
        self.num_inputs = 2;
        self.num_outputs = 2;
        self.num_param_changes = 0;
        self.payload_size = 0;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_pod_sizes() {
        assert_eq!(std::mem::size_of::<ParamChange>(), 8);
        assert_eq!(std::mem::size_of::<ParamMeta>(), 40);
    }
}

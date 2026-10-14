#![allow(dead_code)]

use std::sync::atomic::AtomicU32;

pub const BRIDGE_MAGIC: u32 = 0x5A46_5842; // 'ZFXB'
pub const MAX_FRAMES: usize = 4096;
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
#[derive(Clone, Copy, Default)]
pub struct ParamChange {
    pub id: u32,
    pub value: f32,
}

#[repr(C)]
#[derive(Clone, Copy)]
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

#[repr(C)]
pub struct ZfxSharedBridge {
    pub magic: u32,
    pub host_seq: AtomicU32,
    pub worker_seq: AtomicU32,
    pub command: AtomicU32,
    pub status: AtomicU32,

    // Audio config
    pub sample_rate: f32,
    pub block_size: u32,
    pub num_frames: u32,

    // Host Transport / Tempo sync
    pub tempo: f64,
    pub beat_pos: f64,
    pub time_sig_num: u16,
    pub time_sig_denom: u16,
    pub is_playing: u8,
    pub has_tempo: u8,
    pub pad: [u8; 2],

    // Plugin introspection from worker
    pub num_params: u32,
    pub num_inputs: u32,
    pub num_outputs: u32,
    pub params: [ParamMeta; MAX_PARAMS],

    // Automation / parameter change queue from host
    pub num_param_changes: u32,
    pub param_queue: [ParamChange; MAX_PARAM_QUEUE],

    // Audio streaming buffers (planar stereo f32)
    pub in_l: [f32; MAX_FRAMES],
    pub in_r: [f32; MAX_FRAMES],
    pub out_l: [f32; MAX_FRAMES],
    pub out_r: [f32; MAX_FRAMES],

    // Embedded DLL payload transferred on init
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
/// `val_addr` must point to an initialized `AtomicU32` in valid shared memory.
#[inline]
pub unsafe fn futex_wait(val_addr: *const AtomicU32, expected: u32, timeout_us: Option<u64>) {
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

/// # Safety
/// `val_addr` must point to an initialized `AtomicU32` in valid shared memory.
#[inline]
pub unsafe fn futex_wake(val_addr: *const AtomicU32) {
    libc::syscall(
        libc::SYS_futex,
        val_addr,
        libc::FUTEX_WAKE,
        1,
        std::ptr::null::<libc::timespec>(),
        std::ptr::null::<u32>(),
        0,
    );
}

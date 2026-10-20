//! VST 2.4 host callback, time info, and process globals.
use std::ffi::c_void;
use std::sync::atomic::{AtomicI32, Ordering};

pub static CURRENT_SAMPLE_RATE: AtomicI32 = AtomicI32::new(44100);
pub static CURRENT_BLOCK_SIZE: AtomicI32 = AtomicI32::new(256);

#[repr(C)]
pub struct AEffect {
    pub magic: i32, // 'VstP' (0x56737450)
    pub dispatcher: unsafe extern "C" fn(
        effect: *mut Self,
        opcode: i32,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> isize,
    pub process: unsafe extern "C" fn(
        effect: *mut Self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        sampleframes: i32,
    ),
    pub set_parameter: unsafe extern "C" fn(effect: *mut Self, index: i32, parameter: f32),
    pub get_parameter: unsafe extern "C" fn(effect: *mut Self, index: i32) -> f32,
    pub num_programs: i32,
    pub num_params: i32,
    pub num_inputs: i32,
    pub num_outputs: i32,
    pub flags: i32,
    pub resvd1: isize,
    pub resvd2: isize,
    pub initial_delay: i32,
    pub real_qualities: i32,
    pub off_qualities: i32,
    pub io_ratio: f32,
    pub object: *mut c_void,
    pub user: *mut c_void,
    pub unique_id: i32,
    pub version: i32,
    pub process_replacing: unsafe extern "C" fn(
        effect: *mut Self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        sampleframes: i32,
    ),
    pub process_double_replacing: *mut c_void,
}

// VST 2.4 VstTimeInfo structure for host callbacks (audioMasterGetTime)
#[repr(C)]
pub struct VstTimeInfo {
    pub sample_pos: f64,
    pub sample_rate: f64,
    pub nano_seconds: f64,
    pub ppq_pos: f64,
    pub tempo: f64,
    pub bar_start_pos: f64,
    pub cycle_start_pos: f64,
    pub cycle_end_pos: f64,
    pub time_sig_numerator: i32,
    pub time_sig_denominator: i32,
    pub smpte_offset: i32,
    pub smpte_frame_rate: i32,
    pub samples_to_next_clock: i32,
    pub flags: i32,
}

bitflags::bitflags! {
    /// VST 2.4 VstTimeInfo flags bitmask for `audioMasterGetTime` host callback.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct VstTimeInfoFlags: i32 {
        const TRANSPORT_PLAYING = 1 << 1;  // 0x0002
        const PPQ_POS_VALID     = 1 << 9;  // 0x0200
        const TEMPO_VALID       = 1 << 10; // 0x0400
        const TIME_SIG_VALID    = 1 << 13; // 0x2000
    }
}

pub static mut CURRENT_TIME_INFO: VstTimeInfo = VstTimeInfo {
    sample_pos: 0.0,
    sample_rate: 44100.0,
    nano_seconds: 0.0,
    ppq_pos: 0.0,
    tempo: 120.0,
    bar_start_pos: 0.0,
    cycle_start_pos: 0.0,
    cycle_end_pos: 0.0,
    time_sig_numerator: 4,
    time_sig_denominator: 4,
    smpte_offset: 0,
    smpte_frame_rate: 24,
    samples_to_next_clock: 0,
    flags: VstTimeInfoFlags::TRANSPORT_PLAYING.bits()
        | VstTimeInfoFlags::TEMPO_VALID.bits()
        | VstTimeInfoFlags::PPQ_POS_VALID.bits()
        | VstTimeInfoFlags::TIME_SIG_VALID.bits(),
};

// --- VST 2.4 Host Audio Master Callback ---
#[no_mangle]
pub unsafe extern "C" fn default_audio_master(
    _effect: *mut c_void,
    opcode: i32,
    index: i32,
    value: isize,
    _ptr: *mut c_void,
    opt: f32,
) -> isize {
    crate::worker_log!(
        "[default_audio_master] opcode={opcode}, index={index}, value={value}, opt={opt}"
    );
    match opcode {
        1 => 2400, // audioMasterVersion (VST 2.4)
        7 => {
            // audioMasterGetTime (VST 2.4) - requested by delays, tap-tempo, beat-sync effects
            CURRENT_TIME_INFO.sample_rate = f64::from(CURRENT_SAMPLE_RATE.load(Ordering::Relaxed));
            (&raw const CURRENT_TIME_INFO).addr().cast_signed()
        }
        16 => isize::try_from(CURRENT_SAMPLE_RATE.load(Ordering::Relaxed)).unwrap_or(0), // audioMasterGetSampleRate
        17 => isize::try_from(CURRENT_BLOCK_SIZE.load(Ordering::Relaxed)).unwrap_or(0), // audioMasterGetBlockSize
        _ => 0, // In VST 2.4, unknown/unsupported opcodes MUST return 0 (NULL/false), NEVER 1!
    }
}

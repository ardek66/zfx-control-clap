mod analog_proto;
pub mod dsp_layout;
pub mod log;
mod peldr;
mod protocol;

use std::sync::atomic::Ordering;

struct DspWorkerState {
    _pe: peldr::PeModule,
    inst_l: peldr::ZfxDspInstance,
    inst_r: Option<peldr::ZfxDspInstance>,
    scratch: peldr::AudioScratch,
}

/// Reads an unaligned value of type `T` from a byte offset within an object pointer.
///
/// # Safety
/// The caller must ensure that `base.add(offset)` is valid for reading `size_of::<T>()` bytes.
#[inline]
#[allow(clippy::cast_ptr_alignment)]
const unsafe fn read_offset_unaligned<T: Copy>(base: *mut u8, offset: usize) -> T {
    // SAFETY: Caller guarantees valid pointer range.
    unsafe { std::ptr::read_unaligned(base.add(offset).cast::<T>()) }
}

fn adopt_sample_rate(dsp: &DspWorkerState, sample_rate: f32) -> bool {
    #[allow(clippy::cast_possible_truncation)]
    let sr = sample_rate as i32;
    if sr <= 0 || sr == peldr::CURRENT_SAMPLE_RATE.load(Ordering::Relaxed) {
        return false;
    }
    peldr::CURRENT_SAMPLE_RATE.store(sr, Ordering::Relaxed);
    dsp.inst_l.set_sample_rate(sample_rate);
    if let Some(right) = dsp.inst_r.as_ref() {
        right.set_sample_rate(sample_rate);
    }
    true
}

fn adopt_block_size(dsp: &DspWorkerState, block_size: u32) {
    let bs = i32::try_from(block_size).unwrap_or(0);
    if bs <= 0 || bs == peldr::CURRENT_BLOCK_SIZE.load(Ordering::Relaxed) {
        return;
    }
    peldr::CURRENT_BLOCK_SIZE.store(bs, Ordering::Relaxed);
    dsp.inst_l.set_block_size(bs);
    if let Some(right) = dsp.inst_r.as_ref() {
        right.set_block_size(bs);
    }
}

fn reapply_cached_params(
    dsp: &DspWorkerState,
    param_cache: &[f32; protocol::MAX_PARAMS],
    param_initialized: &[bool; protocol::MAX_PARAMS],
) {
    for p in 0..dsp.inst_l.num_params() {
        let Ok(idx) = usize::try_from(p) else {
            continue;
        };
        if idx >= protocol::MAX_PARAMS || !param_initialized[idx] {
            continue;
        }
        let val = param_cache[idx];
        dsp.inst_l.set_parameter(p, val);
        if let Some(right) = dsp.inst_r.as_ref() {
            right.set_parameter(p, val);
        }
    }
}

fn write_transport(slot: &protocol::ProcessSlot) {
    if slot.has_tempo == 0 {
        return;
    }
    // SAFETY: Writing transport information to the global VST time-info the guest reads.
    unsafe {
        peldr::CURRENT_TIME_INFO.tempo = slot.tempo;
        peldr::CURRENT_TIME_INFO.ppq_pos = slot.beat_pos;
        peldr::CURRENT_TIME_INFO.time_sig_numerator = i32::from(slot.time_sig_num);
        peldr::CURRENT_TIME_INFO.time_sig_denominator = i32::from(slot.time_sig_denom);
        if slot.is_playing != 0 {
            peldr::CURRENT_TIME_INFO.flags |= peldr::VstTimeInfoFlags::TRANSPORT_PLAYING.bits();
        } else {
            peldr::CURRENT_TIME_INFO.flags &= !peldr::VstTimeInfoFlags::TRANSPORT_PLAYING.bits();
        }
    }
}

fn apply_param_queue(
    dsp: &DspWorkerState,
    slot: &protocol::ProcessSlot,
    param_cache: &mut [f32; protocol::MAX_PARAMS],
    param_initialized: &mut [bool; protocol::MAX_PARAMS],
) {
    let max_queue = u32::try_from(protocol::MAX_PARAM_QUEUE).unwrap_or(u32::MAX);
    let num_chgs = usize::try_from(slot.num_param_changes.min(max_queue)).unwrap_or(0);
    for &chg in &slot.param_queue[..num_chgs] {
        let chg_id = i32::try_from(chg.id).unwrap_or(0);
        let idx = usize::try_from(chg_id).unwrap_or(usize::MAX);
        if idx < protocol::MAX_PARAMS {
            param_cache[idx] = chg.value;
            param_initialized[idx] = true;
        }
        dsp.inst_l.set_parameter(chg_id, chg.value);
        if let Some(right) = dsp.inst_r.as_ref() {
            right.set_parameter(chg_id, chg.value);
        }
    }
}

fn render_planes(dsp: &mut DspWorkerState, slot: &mut protocol::ProcessSlot, frames: usize) {
    let DspWorkerState {
        inst_l,
        inst_r,
        scratch,
        ..
    } = dsp;
    match inst_r {
        Some(right) => {
            scratch.render(
                inst_l,
                &[&slot.in_l[..frames]],
                &mut [&mut slot.out_l[..frames]],
            );
            scratch.render(
                right,
                &[&slot.in_r[..frames]],
                &mut [&mut slot.out_r[..frames]],
            );
        }
        None => scratch.render(
            inst_l,
            &[&slot.in_l[..frames], &slot.in_r[..frames]],
            &mut [&mut slot.out_l[..frames], &mut slot.out_r[..frames]],
        ),
    }
}

fn render_slot(
    dsp: &mut DspWorkerState,
    slot: &mut protocol::ProcessSlot,
    param_cache: &mut [f32; protocol::MAX_PARAMS],
    param_initialized: &mut [bool; protocol::MAX_PARAMS],
) {
    if adopt_sample_rate(dsp, slot.sample_rate) {
        reapply_cached_params(dsp, param_cache, param_initialized);
    }
    adopt_block_size(dsp, slot.block_size);
    write_transport(slot);
    apply_param_queue(dsp, slot, param_cache, param_initialized);

    let frames = usize::try_from(slot.num_frames)
        .unwrap_or(0)
        .min(protocol::MAX_FRAMES);
    if frames == 0 {
        return;
    }
    render_planes(dsp, slot, frames);
}

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn main() {
    #[cfg(target_os = "linux")]
    // SAFETY: Installing SIGSEGV crash handler for diagnostic backtraces.
    unsafe {
        extern "C" fn crash_handler(sig: i32, info: *mut libc::siginfo_t, uctx: *mut libc::c_void) {
            // SAFETY: Dereferencing siginfo_t in signal handler to obtain faulting address.
            let addr = unsafe { (*info).si_addr() };
            let ctx = uctx.cast::<libc::ucontext_t>();
            // SAFETY: Reading instruction pointer from valid signal ucontext.
            let eip = unsafe { (*ctx).uc_mcontext.gregs[libc::REG_EIP as usize] };
            eprintln!("[CRASH] Caught signal {sig} at si_addr={addr:?}, EIP={eip:#x}");
            std::process::exit(139);
        }
        let mut sa: libc::sigaction = std::mem::zeroed();
        let handler_addr = crash_handler as *const () as usize;
        sa.sa_sigaction = handler_addr;
        sa.sa_flags = libc::SA_SIGINFO;
        libc::sigaction(libc::SIGSEGV, &raw const sa, std::ptr::null_mut());
    }

    #[cfg(target_arch = "x86")]
    // SAFETY: Setting SSE FTZ (Flush-To-Zero) and DAZ (Denormals-Are-Zero) flags to avoid denormal audio performance penalties.
    unsafe {
        let mut mxcsr: u32 = 0;
        std::arch::asm!("stmxcsr [{}]", in(reg) &raw mut mxcsr, options(nostack));
        mxcsr |= 0x8040; // 0x8000 (FTZ) | 0x0040 (DAZ)
        std::arch::asm!("ldmxcsr [{}]", in(reg) &raw const mxcsr, options(nostack));
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 && args[1] == "--test-dll" {
        let dll_path = &args[2];
        let dll_bytes = match std::fs::read(dll_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[test] Failed to read {dll_path}: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("[test] Loading {} ({} bytes)...", dll_path, dll_bytes.len());
        let pe = match peldr::PeModule::load(&dll_bytes) {
            Ok(pe) => pe,
            Err(e) => {
                eprintln!("[test] PeModule::load failed: {e}");
                std::process::exit(1);
            }
        };
        eprintln!("[test] PeModule loaded! Calling init_zfx_plugin()...");
        let inst = match pe.init_zfx_plugin() {
            Ok(inst) => inst,
            Err(e) => {
                eprintln!("[test] init_zfx_plugin failed: {e}");
                std::process::exit(1);
            }
        };
        eprintln!(
            "[test] SUCCESS! Plugin initialized. num_params={}, num_inputs={}, num_outputs={}",
            inst.num_params(),
            inst.num_inputs(),
            inst.num_outputs()
        );
        inst.open();
        inst.set_sample_rate(44100.0);
        inst.set_block_size(256);
        inst.resume();
        for i in 0..inst.num_params() {
            eprintln!(
                "[test] Param {} ('{}') = {}",
                i,
                inst.get_parameter_name(i),
                inst.get_parameter(i)
            );
        }
        inst.set_parameter(0, 1.0);
        inst.set_parameter(1, 0.5);
        inst.set_parameter(2, 0.5);
        for i in 0..inst.num_params() {
            eprintln!("[test] After set: Param {} = {}", i, inst.get_parameter(i));
        }
        // SAFETY: Diagnostic inspection of internal DSP core object and vtable[6] coefficients.
        unsafe {
            let obj = (*inst.effect).object.cast::<*mut usize>();
            if !obj.is_null() {
                let vtable = *obj;
                eprintln!("[test] obj={obj:?}, vtable={vtable:?}");
                if !vtable.is_null() {
                    let dsp_set_sr_fn = *vtable.add(6);
                    let pe_base = pe.base_addr as usize;
                    eprintln!(
                        "[test] pe_base = {pe_base:#x}, RVA of vtable[6] = {:#x}",
                        dsp_set_sr_fn - pe_base
                    );
                }
                let raw_obj = obj.cast::<u8>();

                inst.set_sample_rate(44100.0);
                eprintln!("[test] --- Object fields at 44.1 kHz ---");
                for offset in (0x180..0x238).step_by(8) {
                    let val = read_offset_unaligned::<f64>(raw_obj, offset);
                    eprintln!("  [obj + {offset:#x}]: {val}");
                }
                eprintln!(
                    "  [obj + 0x228] (int sr): {}",
                    read_offset_unaligned::<i32>(raw_obj, 0x228)
                );
                eprintln!(
                    "  [obj + 0x230] (f64 sr): {}",
                    read_offset_unaligned::<f64>(raw_obj, 0x230)
                );
                eprintln!(
                    "  [obj + 0x250] (comb len): {}",
                    read_offset_unaligned::<u32>(raw_obj, 0x250)
                );

                inst.set_sample_rate(48000.0);
                eprintln!("[test] --- Object fields at 48.0 kHz ---");
                for offset in (0x180..0x238).step_by(8) {
                    let val = read_offset_unaligned::<f64>(raw_obj, offset);
                    eprintln!("  [obj + {offset:#x}]: {val}");
                }
                eprintln!(
                    "  [obj + 0x228] (int sr): {}",
                    read_offset_unaligned::<i32>(raw_obj, 0x228)
                );
                eprintln!(
                    "  [obj + 0x230] (f64 sr): {}",
                    read_offset_unaligned::<f64>(raw_obj, 0x230)
                );
                inst.set_sample_rate(96000.0);
                eprintln!("[test] --- Object fields at 96.0 kHz ---");
                for offset in (0x180..0x238).step_by(8) {
                    let val = read_offset_unaligned::<f64>(raw_obj, offset);
                    eprintln!("  [obj + {offset:#x}]: {val}");
                }
                eprintln!(
                    "  [obj + 0x228] (int sr): {}",
                    read_offset_unaligned::<i32>(raw_obj, 0x228)
                );
                eprintln!(
                    "  [obj + 0x230] (f64 sr): {}",
                    read_offset_unaligned::<f64>(raw_obj, 0x230)
                );
                eprintln!(
                    "  [obj + 0x250] (comb len): {}",
                    read_offset_unaligned::<u32>(raw_obj, 0x250)
                );
            }
        }
        let is_mono_mono = inst.num_inputs() == 1 && inst.num_outputs() == 1;
        let inst2 = if is_mono_mono {
            match pe.init_zfx_plugin() {
                Ok(inst2) => {
                    inst2.open();
                    inst2.set_sample_rate(44100.0);
                    inst2.set_block_size(256);
                    inst2.resume();
                    inst2.set_parameter(0, 1.0);
                    inst2.set_parameter(1, 0.5);
                    inst2.set_parameter(2, 0.5);
                    Some(inst2)
                }
                Err(e) => {
                    eprintln!("[test] Dual instance init failed: {e}");
                    None
                }
            }
        } else {
            None
        };

        let mut scratch = peldr::AudioScratch::new(
            usize::try_from(inst.num_inputs()).unwrap_or(2),
            usize::try_from(inst.num_outputs()).unwrap_or(2),
        );

        let in_buf_l: Vec<f32> = (0..256)
            .map(|i| (f32::from(u8::try_from(i).unwrap()) * 0.1_f32).sin() * 0.5_f32)
            .collect();
        let in_buf_r: Vec<f32> = (0..256)
            .map(|i| (f32::from(u8::try_from(i).unwrap()) * 0.2_f32).cos() * 0.3_f32)
            .collect();
        let mut out_l = vec![0.0f32; 256];
        let mut out_r = vec![0.0f32; 256];

        if let Some(ref inst_r) = inst2 {
            scratch.render(&inst, &[&in_buf_l], &mut [&mut out_l]);
            scratch.render(inst_r, &[&in_buf_r], &mut [&mut out_r]);
            eprintln!(
                "[test] Dual-DSP success! L non-zeros={}/256, R non-zeros={}/256",
                out_l.iter().filter(|&&x| x.abs() > 1e-6).count(),
                out_r.iter().filter(|&&x| x.abs() > 1e-6).count()
            );
        } else {
            scratch.render(
                &inst,
                &[&in_buf_l, &in_buf_r],
                &mut [&mut out_l, &mut out_r],
            );
            eprintln!(
                "[test] Native stereo process success! L non-zeros={}/256, R non-zeros={}/256",
                out_l.iter().filter(|&&x| x.abs() > 1e-6).count(),
                out_r.iter().filter(|&&x| x.abs() > 1e-6).count()
            );
        }
        eprintln!(
            "[test] L first 4={:?}, R first 4={:?}",
            &out_l[..4],
            &out_r[..4]
        );
        eprintln!("\n[test] =========================================");
        eprintln!("[test] Multi-Rate Frequency Response Validation");
        eprintln!("[test] =========================================");

        let test_freqs = [100.0_f32, 500.0_f32, 1000.0_f32, 2500.0_f32, 5000.0_f32];
        let meas_start = 8192;
        let meas_len = 8192.0_f32;

        // Helper to process audio in standard 256-sample blocks.
        // Scoped so the closure releases `scratch` before the benchmark below.
        {
        let mut run_steady_sine = |target_sr: f32, freq: f32| -> f32 {
            inst.set_sample_rate(target_sr);
            inst.set_block_size(256);
            inst.resume();
            for p in 0..inst.num_params() {
                inst.set_parameter(p, inst.get_parameter(p));
            }

            let total_frames = 16384;
            let in_sine: Vec<f32> = (0..total_frames)
                .map(|i| {
                    let t = f32::from(u16::try_from(i).unwrap()) / target_sr;
                    (2.0 * std::f32::consts::PI * freq * t).sin() * 0.25
                })
                .collect();
            let mut out = vec![0.0f32; total_frames];

            for start in (0..total_frames).step_by(256) {
                let end = (start + 256).min(total_frames);
                scratch.render(&inst, &[&in_sine[start..end]], &mut [&mut out[start..end]]);
            }

            let in_rms =
                (in_sine[meas_start..].iter().map(|&x| x * x).sum::<f32>() / meas_len).sqrt();
            let out_rms = (out[meas_start..].iter().map(|&x| x * x).sum::<f32>() / meas_len).sqrt();
            20.0 * (out_rms / in_rms.max(1e-12)).log10()
        };

        // 1. Reference Run at 48.0 kHz
        let mut ref_gains_db = Vec::new();
        for &freq in &test_freqs {
            let gain_db = run_steady_sine(48000.0, freq);
            ref_gains_db.push(gain_db);
            eprintln!("[test]  48k Ref:        Freq {freq:5.0} Hz -> Gain = {gain_db:+6.2} dB");
        }

        // 2. Option B Adapted 96.0 kHz Run
        for (idx, &freq) in test_freqs.iter().enumerate() {
            let gain_db = run_steady_sine(96000.0, freq);
            let err = gain_db - ref_gains_db[idx];
            eprintln!("[test]  96k Option B:   Freq {freq:5.0} Hz -> Gain = {gain_db:+6.2} dB (Error: {err:+6.2} dB)");
        }

        // 3. Option B Adapted 192.0 kHz Run
        for (idx, &freq) in test_freqs.iter().enumerate() {
            let gain_db = run_steady_sine(192_000.0, freq);
            let err = gain_db - ref_gains_db[idx];
            eprintln!("[test] 192k Option B:   Freq {freq:5.0} Hz -> Gain = {gain_db:+6.2} dB (Error: {err:+6.2} dB)");
        }
        }
        eprintln!("[test] =========================================");
        eprintln!("[test] Parameter 4 Rocking Latency Benchmark");
        eprintln!("[test] =========================================");
        for &(sr, block_size) in &[
            (48_000.0_f32, 512_usize),
            (96_000.0_f32, 1024_usize),
            (192_000.0_f32, 2048_usize),
            (192_000.0_f32, 4096_usize),
        ] {
            inst.set_sample_rate(sr);
            let in_buf: Vec<f32> = (0..block_size)
                .map(|j| {
                    (f32::from(u16::try_from(j).unwrap()) * 220.0 * 2.0 * std::f32::consts::PI
                        / sr)
                        .sin()
                        * 0.5
                })
                .collect();
            let mut out_buf = vec![0.0f32; block_size];
            let mut total_time = std::time::Duration::ZERO;
            for i in 0..50_u16 {
                let pedal_pos = (f32::from(i) * 0.05_f32).sin().abs();
                inst.set_parameter(4, pedal_pos);
                let start = std::time::Instant::now();
                scratch.render(&inst, &[&in_buf], &mut [&mut out_buf]);
                total_time += start.elapsed();
            }
            let avg_ms = total_time.as_secs_f64() * 1000.0 / 50.0;
            let block_period_ms =
                f64::from(u32::try_from(block_size).unwrap()) / f64::from(sr) * 1000.0;
            eprintln!(
                "[test] SR {:6.0} Hz, Block {:4}: avg {:6.3} ms / block (deadline: {:6.3} ms, CPU load: {:5.1}%)",
                sr,
                block_size,
                avg_ms,
                block_period_ms,
                (avg_ms / block_period_ms) * 100.0
            );
        }
        eprintln!("[test] =========================================\n");

        return;
    }

    let bridge_size = std::mem::size_of::<protocol::ZfxSharedBridge>();
    #[cfg(target_os = "linux")]
    let addr = {
        let fd: i32 = if args.len() > 1 {
            args[1].parse().unwrap_or(3)
        } else {
            3
        };
        // SAFETY: Mapping shared memory region allocated by host bootstrap process via file descriptor `fd`.
        let mapped = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bridge_size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if mapped == libc::MAP_FAILED {
            worker_log!("[zfx-worker32] Failed to mmap shared memory fd {fd}");
            std::process::exit(1);
        }
        mapped
    };

    #[cfg(target_os = "windows")]
    let addr = {
        use windows_sys::Win32::System::Memory::{
            MapViewOfFile, OpenFileMappingA, FILE_MAP_ALL_ACCESS,
        };
        let map_name = if args.len() > 1 {
            &args[1]
        } else {
            "Local\\ZfxBridge_default"
        };
        let mut map_name_bytes = map_name.as_bytes().to_vec();
        map_name_bytes.push(0);
        // SAFETY: Opening named file mapping provided on command line by host process.
        let h_map = unsafe { OpenFileMappingA(FILE_MAP_ALL_ACCESS, 0, map_name_bytes.as_ptr()) };
        if h_map.is_null() {
            worker_log!("[zfx-worker32] Failed to OpenFileMappingA {map_name}");
            std::process::exit(1);
        }
        // SAFETY: Mapping shared memory view from file mapping handle.
        let mapped = unsafe { MapViewOfFile(h_map, FILE_MAP_ALL_ACCESS, 0, 0, bridge_size) };
        if mapped.Value.is_null() {
            worker_log!("[zfx-worker32] Failed to MapViewOfFile for {map_name}");
            std::process::exit(1);
        }
        mapped.Value
    };

    // SAFETY: `addr` is verified mapped and points to a valid `ZfxSharedBridge` instance.
    let bridge = unsafe { &mut *addr.cast::<protocol::ZfxSharedBridge>() };

    if bridge.magic != protocol::BRIDGE_MAGIC {
        worker_log!("[zfx-worker32] Bridge magic mismatch: {:#x}", bridge.magic);
        std::process::exit(2);
    }

    // Signal ready for commands
    bridge
        .status
        .store(protocol::STATUS_READY, Ordering::Release);
    // SAFETY: Signaling status futex to notify bootstrap parent.
    unsafe {
        protocol::futex_wake(&raw const bridge.status);
    }

    let mut dsp_state: Option<DspWorkerState> = None;
    let mut param_cache = [0.0f32; protocol::MAX_PARAMS];
    let mut param_initialized = [false; protocol::MAX_PARAMS];

    loop {
        let host_seq = bridge.host_seq.load(Ordering::Acquire);
        let worker_seq = bridge.worker_seq.load(Ordering::Relaxed);

        if host_seq == worker_seq {
            if let Some(mut claimed) = bridge.claim_block() {
                if let Some(dsp) = dsp_state.as_mut() {
                    render_slot(
                        dsp,
                        claimed.slot(),
                        &mut param_cache,
                        &mut param_initialized,
                    );
                    bridge.status.store(protocol::STATUS_OK, Ordering::Release);
                } else {
                    bridge.status.store(protocol::STATUS_ERROR, Ordering::Release);
                }
                continue;
            }
            let bell = bridge.doorbell.load(Ordering::Acquire);
            let submit = bridge.submit_seq.load(Ordering::Acquire);
            let done = bridge.done_seq.load(Ordering::Relaxed);
            if bridge.host_seq.load(Ordering::Acquire) != worker_seq || submit != done {
                continue;
            }
            // SAFETY: `doorbell` is the bridge's wake word. The worker rechecks
            // both sequences after loading it, so a missed wake is retried.
            unsafe {
                protocol::futex_wait(&raw const bridge.doorbell, bell, Some(100_000));
            }
            continue;
        }

        let cmd = bridge.command.load(Ordering::Acquire);
        match cmd {
            protocol::CMD_INIT => {
                let psize = usize::try_from(bridge.payload_size).unwrap_or(0);
                let payload = &bridge.payload_bytes[..psize];
                #[allow(clippy::cast_possible_truncation)]
                let sample_rate_i32 = bridge.sample_rate as i32;
                peldr::CURRENT_SAMPLE_RATE.store(sample_rate_i32, Ordering::Relaxed);
                let block_size_i32 = i32::try_from(bridge.block_size).unwrap_or(256);
                peldr::CURRENT_BLOCK_SIZE.store(block_size_i32, Ordering::Relaxed);
                match peldr::PeModule::load(payload) {
                    Ok(pe) => match pe.init_zfx_plugin() {
                        Ok(inst_l) => {
                            inst_l.open();
                            inst_l.set_sample_rate(bridge.sample_rate);
                            inst_l.set_block_size(block_size_i32);
                            inst_l.resume();

                            let inst_r = if inst_l.num_inputs() == 1 && inst_l.num_outputs() == 1 {
                                match pe.init_zfx_plugin() {
                                    Ok(inst_r) => {
                                        inst_r.open();
                                        inst_r.set_sample_rate(bridge.sample_rate);
                                        inst_r.set_block_size(block_size_i32);
                                        inst_r.resume();
                                        Some(inst_r)
                                    }
                                    Err(e) => {
                                        worker_log!("[zfx-worker32] Failed to init secondary DSP instance: {e}");
                                        None
                                    }
                                }
                            } else {
                                None
                            };

                            bridge.num_params = u32::try_from(inst_l.num_params()).unwrap_or(0);
                            bridge.num_inputs = u32::try_from(inst_l.num_inputs()).unwrap_or(0);
                            bridge.num_outputs = u32::try_from(inst_l.num_outputs()).unwrap_or(0);

                            for (i, param_slot) in
                                (0..inst_l.num_params()).zip(bridge.params.iter_mut())
                            {
                                let name = inst_l.get_parameter_name(i);
                                let name_bytes = name.as_bytes();
                                let copy_len = name_bytes.len().min(31);
                                let param_id = u32::try_from(i).unwrap_or(0);
                                let mut meta = protocol::ParamMeta {
                                    id: param_id,
                                    default_val: inst_l.get_parameter(i),
                                    ..Default::default()
                                };
                                meta.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
                                *param_slot = meta;
                            }

                            for p in 0..inst_l.num_params() {
                                if let Ok(idx) = usize::try_from(p) {
                                    if idx < protocol::MAX_PARAMS {
                                        param_cache[idx] = inst_l.get_parameter(p);
                                        param_initialized[idx] = true;
                                    }
                                }
                            }

                            let scratch = peldr::AudioScratch::new(
                                usize::try_from(inst_l.num_inputs()).unwrap_or(2),
                                usize::try_from(inst_l.num_outputs()).unwrap_or(2),
                            );
                            dsp_state = Some(DspWorkerState {
                                _pe: pe,
                                inst_l,
                                inst_r,
                                scratch,
                            });
                            bridge.status.store(protocol::STATUS_OK, Ordering::Release);
                        }
                        Err(e) => {
                            worker_log!("[zfx-worker32] init_zfx_plugin error: {e}");
                            bridge
                                .status
                                .store(protocol::STATUS_ERROR, Ordering::Release);
                        }
                    },
                    Err(e) => {
                        worker_log!("[zfx-worker32] PeModule::load error: {e}");
                        bridge
                            .status
                            .store(protocol::STATUS_ERROR, Ordering::Release);
                    }
                }
                bridge.worker_seq.store(host_seq, Ordering::Release);
                // SAFETY: Waking host bootstrap waiting on worker_seq futex.
                unsafe {
                    protocol::futex_wake(&raw const bridge.worker_seq);
                }
            }

            protocol::CMD_SHUTDOWN => {
                if let Some(ref dsp) = dsp_state {
                    dsp.inst_l.suspend();
                    dsp.inst_l.close();
                    if let Some(ref r) = dsp.inst_r {
                        r.suspend();
                        r.close();
                    }
                }
                bridge.status.store(protocol::STATUS_OK, Ordering::Release);
                bridge.worker_seq.store(host_seq, Ordering::Release);
                // SAFETY: Waking host bootstrap waiting on worker_seq futex.
                unsafe {
                    protocol::futex_wake(&raw const bridge.worker_seq);
                }
                break;
            }

            _ => {
                bridge.worker_seq.store(host_seq, Ordering::Release);
                // SAFETY: Waking host bootstrap waiting on worker_seq futex.
                unsafe {
                    protocol::futex_wake(&raw const bridge.worker_seq);
                }
            }
        }
    }
}

#[cfg(all(target_os = "windows", target_env = "gnu"))]
#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    std::process::abort();
}

mod peldr;
mod protocol;

use std::sync::atomic::Ordering;

struct DspWorkerState {
    _pe: peldr::PeModule,
    inst_l: peldr::ZfxDspInstance,
    inst_r: Option<peldr::ZfxDspInstance>,
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

#[allow(clippy::too_many_lines, clippy::cognitive_complexity)]
fn main() {
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
        // SAFETY: Diagnostic inspection of internal VST 2.4 structure offsets for debugging.
        unsafe {
            let obj = (*inst.effect).object.cast::<u8>();
            eprintln!(
                "[test] Internal offsets: 0x170={:.4}, 0x17c={:#x}, 0x190={}, 0x194={:#x}, 0x198={:?}, 0x19c={:.4}, 0x1c0={:.4}, 0x1cc={:.4}",
                read_offset_unaligned::<f64>(obj, 0x170),
                read_offset_unaligned::<usize>(obj, 0x17c),
                *obj.add(0x190),
                read_offset_unaligned::<usize>(obj, 0x194),
                read_offset_unaligned::<*const f64>(obj, 0x198),
                read_offset_unaligned::<f32>(obj, 0x19c),
                read_offset_unaligned::<f32>(obj, 0x1c0),
                read_offset_unaligned::<f32>(obj, 0x1cc),
            );
            let buf_198 = read_offset_unaligned::<*const f64>(obj, 0x198);
            if !buf_198.is_null() {
                let s_198 = std::slice::from_raw_parts(buf_198, 8);
                eprintln!("[test] First 8 elements of offset_198 buffer: {s_198:?}");
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

        let in_buf_l: Vec<f32> = (0..256)
            .map(|i| (f32::from(u8::try_from(i).unwrap()) * 0.1_f32).sin() * 0.5_f32)
            .collect();
        let in_buf_r: Vec<f32> = (0..256)
            .map(|i| (f32::from(u8::try_from(i).unwrap()) * 0.2_f32).cos() * 0.3_f32)
            .collect();
        let mut out_l = vec![0.0f32; 256];
        let mut out_r = vec![0.0f32; 256];

        if let Some(ref inst_r) = inst2 {
            inst.process_audio(&[&in_buf_l], &mut [&mut out_l], 256);
            inst_r.process_audio(&[&in_buf_r], &mut [&mut out_r], 256);
            eprintln!(
                "[test] Dual-DSP success! L non-zeros={}/256, R non-zeros={}/256",
                out_l.iter().filter(|&&x| x.abs() > 1e-6).count(),
                out_r.iter().filter(|&&x| x.abs() > 1e-6).count()
            );
        } else {
            inst.process_audio(&[&in_buf_l, &in_buf_r], &mut [&mut out_l, &mut out_r], 256);
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
        let non_zeros = out_l.iter().filter(|&&x| x.abs() > 1e-6).count();
        eprintln!(
            "[test] Processed 256 samples: non_zeros={}/256, min={:?}, max={:?}, first 4={:?}",
            non_zeros,
            out_l.iter().copied().fold(f32::INFINITY, f32::min),
            out_l.iter().copied().fold(f32::NEG_INFINITY, f32::max),
            &out_l[..4]
        );
        return;
    }

    let fd: i32 = if args.len() > 1 {
        args[1].parse().unwrap_or(3)
    } else {
        3
    };

    let bridge_size = std::mem::size_of::<protocol::ZfxSharedBridge>();
    // SAFETY: Mapping shared memory region allocated by host bootstrap process via file descriptor `fd`.
    let addr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bridge_size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };

    if addr == libc::MAP_FAILED {
        eprintln!("[zfx-worker32] Failed to mmap shared memory fd {fd}");
        std::process::exit(1);
    }

    // SAFETY: `addr` is verified mapped and points to a valid `ZfxSharedBridge` instance.
    let bridge = unsafe { &mut *addr.cast::<protocol::ZfxSharedBridge>() };

    if bridge.magic != protocol::BRIDGE_MAGIC {
        eprintln!("[zfx-worker32] Bridge magic mismatch: {:#x}", bridge.magic);
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

    loop {
        let host_seq = bridge.host_seq.load(Ordering::Acquire);
        let worker_seq = bridge.worker_seq.load(Ordering::Relaxed);

        if host_seq == worker_seq {
            // Idle wait: sleep on futex until host_seq changes
            // SAFETY: Waiting on host_seq futex until host issues a new command.
            unsafe {
                protocol::futex_wait(&raw const bridge.host_seq, host_seq, Some(100_000));
            }
            continue;
        }

        let cmd = bridge.command.load(Ordering::Acquire);
        match cmd {
            protocol::CMD_INIT => {
                let psize = usize::try_from(bridge.payload_size).unwrap_or(0);
                let payload = &bridge.payload_bytes[..psize];
                #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
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
                                        eprintln!("[zfx-worker32] Failed to init secondary DSP instance: {e}");
                                        None
                                    }
                                }
                            } else {
                                None
                            };

                            bridge.num_params = u32::try_from(inst_l.num_params()).unwrap_or(0);
                            bridge.num_inputs = u32::try_from(inst_l.num_inputs()).unwrap_or(0);
                            bridge.num_outputs = u32::try_from(inst_l.num_outputs()).unwrap_or(0);

                            for i in 0..inst_l.num_params() {
                                let name = inst_l.get_parameter_name(i);
                                let name_bytes = name.as_bytes();
                                let copy_len = name_bytes.len().min(31);
                                let param_id = u32::try_from(i).unwrap_or(0);
                                let param_idx = usize::try_from(i).unwrap_or(usize::MAX);
                                let mut meta = protocol::ParamMeta {
                                    id: param_id,
                                    default_val: inst_l.get_parameter(i),
                                    ..Default::default()
                                };
                                meta.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
                                if param_idx < protocol::MAX_PARAMS {
                                    bridge.params[param_idx] = meta;
                                }
                            }

                            dsp_state = Some(DspWorkerState {
                                _pe: pe,
                                inst_l,
                                inst_r,
                            });
                            bridge.status.store(protocol::STATUS_OK, Ordering::Release);
                        }
                        Err(e) => {
                            eprintln!("[zfx-worker32] init_zfx_plugin error: {e}");
                            bridge
                                .status
                                .store(protocol::STATUS_ERROR, Ordering::Release);
                        }
                    },
                    Err(e) => {
                        eprintln!("[zfx-worker32] PeModule::load error: {e}");
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

            protocol::CMD_PROCESS => {
                if let Some(ref dsp) = dsp_state {
                    // Update sample rate / block size if host updated them in activate
                    #[allow(clippy::as_conversions, clippy::cast_possible_truncation)]
                    let sr = bridge.sample_rate as i32;
                    let bs = i32::try_from(bridge.block_size).unwrap_or(256);
                    if sr > 0 && sr != peldr::CURRENT_SAMPLE_RATE.load(Ordering::Relaxed) {
                        peldr::CURRENT_SAMPLE_RATE.store(sr, Ordering::Relaxed);
                        dsp.inst_l.set_sample_rate(bridge.sample_rate);
                        if let Some(ref r) = dsp.inst_r {
                            r.set_sample_rate(bridge.sample_rate);
                        }
                    }
                    if bs > 0 && bs != peldr::CURRENT_BLOCK_SIZE.load(Ordering::Relaxed) {
                        peldr::CURRENT_BLOCK_SIZE.store(bs, Ordering::Relaxed);
                        dsp.inst_l.set_block_size(bs);
                        if let Some(ref r) = dsp.inst_r {
                            r.set_block_size(bs);
                        }
                    }

                    // Update host transport & tempo
                    if bridge.has_tempo != 0 {
                        // SAFETY: Writing transport information to global static CURRENT_TIME_INFO.
                        unsafe {
                            peldr::CURRENT_TIME_INFO.tempo = bridge.tempo;
                            peldr::CURRENT_TIME_INFO.ppq_pos = bridge.beat_pos;
                            peldr::CURRENT_TIME_INFO.time_sig_numerator =
                                i32::from(bridge.time_sig_num);
                            peldr::CURRENT_TIME_INFO.time_sig_denominator =
                                i32::from(bridge.time_sig_denom);
                            if bridge.is_playing != 0 {
                                peldr::CURRENT_TIME_INFO.flags |= 0x0002;
                            } else {
                                peldr::CURRENT_TIME_INFO.flags &= !0x0002;
                            }
                        }
                    }

                    // 1. Process queued parameter updates
                    let max_queue = u32::try_from(protocol::MAX_PARAM_QUEUE).unwrap_or(u32::MAX);
                    let num_chgs =
                        usize::try_from(bridge.num_param_changes.min(max_queue)).unwrap_or(0);
                    for i in 0..num_chgs {
                        let chg = bridge.param_queue[i];
                        let chg_id = i32::try_from(chg.id).unwrap_or(0);
                        dsp.inst_l.set_parameter(chg_id, chg.value);
                        if let Some(ref r) = dsp.inst_r {
                            r.set_parameter(chg_id, chg.value);
                        }
                    }

                    // 2. Process audio
                    let frames = usize::try_from(bridge.num_frames)
                        .unwrap_or(0)
                        .min(protocol::MAX_FRAMES);
                    if frames > 0 {
                        // SAFETY: `addr` points to valid mapped shared memory containing planar audio buffers.
                        let (audio_in_left, audio_in_right, audio_out_left, audio_out_right) = unsafe {
                            let b_ptr = addr.cast::<protocol::ZfxSharedBridge>();
                            (
                                std::slice::from_raw_parts((*b_ptr).in_l.as_ptr(), frames),
                                std::slice::from_raw_parts((*b_ptr).in_r.as_ptr(), frames),
                                std::slice::from_raw_parts_mut((*b_ptr).out_l.as_mut_ptr(), frames),
                                std::slice::from_raw_parts_mut((*b_ptr).out_r.as_mut_ptr(), frames),
                            )
                        };

                        if let Some(ref r) = dsp.inst_r {
                            // Dual-mono True Stereo processing:
                            // Channel 0 (Left) -> inst_l -> Channel 0 (Left)
                            // Channel 1 (Right) -> inst_r -> Channel 1 (Right)
                            dsp.inst_l.process_audio(
                                &[audio_in_left],
                                &mut [audio_out_left],
                                frames,
                            );
                            r.process_audio(&[audio_in_right], &mut [audio_out_right], frames);
                        } else {
                            dsp.inst_l.process_audio(
                                &[audio_in_left, audio_in_right],
                                &mut [audio_out_left, audio_out_right],
                                frames,
                            );
                        }
                    }
                    bridge.status.store(protocol::STATUS_OK, Ordering::Release);
                } else {
                    bridge
                        .status
                        .store(protocol::STATUS_ERROR, Ordering::Release);
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

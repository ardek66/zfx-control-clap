mod peldr;
mod protocol;

use std::sync::atomic::Ordering;

fn main() {
    #[cfg(target_arch = "x86")]
    unsafe {
        let mxcsr = std::arch::x86::_mm_getcsr();
        std::arch::x86::_mm_setcsr(mxcsr | 0x8040); // 0x8000 (FTZ) | 0x0040 (DAZ)
    }

    let args: Vec<String> = std::env::args().collect();
    if args.len() > 2 && args[1] == "--test-dll" {
        let dll_path = &args[2];
        let dll_bytes = match std::fs::read(dll_path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("[test] Failed to read {}: {}", dll_path, e);
                std::process::exit(1);
            }
        };
        eprintln!("[test] Loading {} ({} bytes)...", dll_path, dll_bytes.len());
        let pe = match peldr::PeModule::load(&dll_bytes) {
            Ok(pe) => pe,
            Err(e) => {
                eprintln!("[test] PeModule::load failed: {}", e);
                std::process::exit(1);
            }
        };
        eprintln!("[test] PeModule loaded! Calling init_zfx_plugin()...");
        let inst = match pe.init_zfx_plugin() {
            Ok(inst) => inst,
            Err(e) => {
                eprintln!("[test] init_zfx_plugin failed: {}", e);
                std::process::exit(1);
            }
        };
        eprintln!("[test] SUCCESS! Plugin initialized. num_params={}, num_inputs={}, num_outputs={}",
            inst.num_params(), inst.num_inputs(), inst.num_outputs());
        inst.open();
        inst.set_sample_rate(44100.0);
        inst.set_block_size(256);
        inst.resume();
        for i in 0..inst.num_params() {
            eprintln!("[test] Param {} ('{}') = {}", i, inst.get_parameter_name(i), inst.get_parameter(i));
        }
        inst.set_parameter(0, 1.0);
        inst.set_parameter(1, 0.5);
        inst.set_parameter(2, 0.5);
        for i in 0..inst.num_params() {
            eprintln!("[test] After set: Param {} = {}", i, inst.get_parameter(i));
        }
        unsafe {
            let obj = (*inst.effect).object as *const u8;
            let p_170 = *(obj.add(0x170) as *const f64);
            let p_17c = *(obj.add(0x17c) as *const usize);
            let p_190 = *obj.add(0x190);
            let p_194 = *(obj.add(0x194) as *const usize);
            let p_198 = *(obj.add(0x198) as *const *const f64);
            let p_19c = *(obj.add(0x19c) as *const f32);
            let p_1c0 = *(obj.add(0x1c0) as *const f32);
            let p_1cc = *(obj.add(0x1cc) as *const f32);
            eprintln!("[test] Internal offsets: 0x170={:.4}, 0x17c={:#x}, 0x190={}, 0x194={:#x}, 0x198={:?}, 0x19c={:.4}, 0x1c0={:.4}, 0x1cc={:.4}",
                p_170, p_17c, p_190, p_194, p_198, p_19c, p_1c0, p_1cc);
            if !p_198.is_null() {
                let s_198 = std::slice::from_raw_parts(p_198, 8);
                eprintln!("[test] First 8 elements of p_198 buffer: {:?}", s_198);
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
                    eprintln!("[test] Dual instance init failed: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let in_buf_l: Vec<f32> = (0..256).map(|i| (i as f32 * 0.1).sin() * 0.5).collect();
        let in_buf_r: Vec<f32> = (0..256).map(|i| (i as f32 * 0.2).cos() * 0.3).collect();
        let mut out_l = vec![0.0f32; 256];
        let mut out_r = vec![0.0f32; 256];

        if let Some(ref inst_r) = inst2 {
            inst.process_audio(&[&in_buf_l], &mut [&mut out_l], 256);
            inst_r.process_audio(&[&in_buf_r], &mut [&mut out_r], 256);
            eprintln!("[test] Dual-DSP success! L non-zeros={}/256, R non-zeros={}/256",
                out_l.iter().filter(|&&x| x.abs() > 1e-6).count(),
                out_r.iter().filter(|&&x| x.abs() > 1e-6).count());
            eprintln!("[test] L first 4={:?}, R first 4={:?}", &out_l[..4], &out_r[..4]);
        } else {
            inst.process_audio(&[&in_buf_l, &in_buf_r], &mut [&mut out_l, &mut out_r], 256);
            eprintln!("[test] Native stereo process success! L non-zeros={}/256, R non-zeros={}/256",
                out_l.iter().filter(|&&x| x.abs() > 1e-6).count(),
                out_r.iter().filter(|&&x| x.abs() > 1e-6).count());
            eprintln!("[test] L first 4={:?}, R first 4={:?}", &out_l[..4], &out_r[..4]);
        }
        let non_zeros = out_l.iter().filter(|&&x| x.abs() > 1e-6).count();
        eprintln!("[test] Processed 256 samples: non_zeros={}/256, min={:?}, max={:?}, first 4={:?}",
            non_zeros,
            out_l.iter().cloned().fold(f32::INFINITY, f32::min),
            out_l.iter().cloned().fold(f32::NEG_INFINITY, f32::max),
            &out_l[..4]);
        return;
    }

    let fd: i32 = if args.len() > 1 {
        args[1].parse().unwrap_or(3)
    } else {
        3
    };

    let bridge_size = std::mem::size_of::<protocol::ZfxSharedBridge>();
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
        eprintln!("[zfx-worker32] Failed to mmap shared memory fd {}", fd);
        std::process::exit(1);
    }

    let bridge = unsafe { &mut *(addr as *mut protocol::ZfxSharedBridge) };

    if bridge.magic != protocol::BRIDGE_MAGIC {
        eprintln!("[zfx-worker32] Bridge magic mismatch: {:#x}", bridge.magic);
        std::process::exit(2);
    }

    // Signal ready for commands
    bridge.status.store(protocol::STATUS_READY, Ordering::Release);
    unsafe {
        protocol::futex_wake(&bridge.status);
    }

    struct DspWorkerState {
        _pe: peldr::PeModule,
        inst_l: peldr::ZfxDspInstance,
        inst_r: Option<peldr::ZfxDspInstance>,
    }

    let mut dsp_state: Option<DspWorkerState> = None;

    loop {
        let host_seq = bridge.host_seq.load(Ordering::Acquire);
        let worker_seq = bridge.worker_seq.load(Ordering::Relaxed);

        if host_seq == worker_seq {
            // Idle wait: sleep on futex until host_seq changes
            unsafe {
                protocol::futex_wait(&bridge.host_seq, host_seq, Some(100_000)); // 100ms timeout
            }
            continue;
        }

        let cmd = bridge.command.load(Ordering::Acquire);
        match cmd {
            protocol::CMD_INIT => {
                let psize = bridge.payload_size as usize;
                let payload = &bridge.payload_bytes[..psize];
                peldr::CURRENT_SAMPLE_RATE.store(bridge.sample_rate as i32, Ordering::Relaxed);
                peldr::CURRENT_BLOCK_SIZE.store(bridge.block_size as i32, Ordering::Relaxed);
                match peldr::PeModule::load(payload) {
                    Ok(pe) => match pe.init_zfx_plugin() {
                        Ok(inst_l) => {
                            inst_l.open();
                            inst_l.set_sample_rate(bridge.sample_rate);
                            inst_l.set_block_size(bridge.block_size as i32);
                            inst_l.resume();

                            let inst_r = if inst_l.num_inputs() == 1 && inst_l.num_outputs() == 1 {
                                match pe.init_zfx_plugin() {
                                    Ok(inst_r) => {
                                        inst_r.open();
                                        inst_r.set_sample_rate(bridge.sample_rate);
                                        inst_r.set_block_size(bridge.block_size as i32);
                                        inst_r.resume();
                                        Some(inst_r)
                                    }
                                    Err(e) => {
                                        eprintln!("[zfx-worker32] Failed to init secondary DSP instance: {}", e);
                                        None
                                    }
                                }
                            } else {
                                None
                            };

                            bridge.num_params = inst_l.num_params() as u32;
                            bridge.num_inputs = inst_l.num_inputs() as u32;
                            bridge.num_outputs = inst_l.num_outputs() as u32;

                            for i in 0..inst_l.num_params() {
                                let mut meta = protocol::ParamMeta::default();
                                meta.id = i as u32;
                                let name = inst_l.get_parameter_name(i);
                                let name_bytes = name.as_bytes();
                                let copy_len = name_bytes.len().min(31);
                                meta.name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
                                meta.default_val = inst_l.get_parameter(i);
                                if (i as usize) < protocol::MAX_PARAMS {
                                    bridge.params[i as usize] = meta;
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
                            eprintln!("[zfx-worker32] init_zfx_plugin error: {}", e);
                            bridge.status.store(protocol::STATUS_ERROR, Ordering::Release);
                        }
                    },
                    Err(e) => {
                        eprintln!("[zfx-worker32] PeModule::load error: {}", e);
                        bridge.status.store(protocol::STATUS_ERROR, Ordering::Release);
                    }
                }
                bridge.worker_seq.store(host_seq, Ordering::Release);
                unsafe {
                    protocol::futex_wake(&bridge.worker_seq);
                }
            }

            protocol::CMD_PROCESS => {
                if let Some(ref dsp) = dsp_state {
                    // Update sample rate / block size if host updated them in activate
                    let sr = bridge.sample_rate as i32;
                    let bs = bridge.block_size as i32;
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
                        unsafe {
                            peldr::CURRENT_TIME_INFO.tempo = bridge.tempo;
                            peldr::CURRENT_TIME_INFO.ppq_pos = bridge.beat_pos;
                            peldr::CURRENT_TIME_INFO.time_sig_numerator = bridge.time_sig_num as i32;
                            peldr::CURRENT_TIME_INFO.time_sig_denominator = bridge.time_sig_denom as i32;
                            if bridge.is_playing != 0 {
                                peldr::CURRENT_TIME_INFO.flags |= 0x0002;
                            } else {
                                peldr::CURRENT_TIME_INFO.flags &= !0x0002;
                            }
                        }
                    }

                    // 1. Process queued parameter updates
                    let num_chgs = bridge.num_param_changes.min(protocol::MAX_PARAM_QUEUE as u32);
                    for i in 0..num_chgs as usize {
                        let chg = bridge.param_queue[i];
                        dsp.inst_l.set_parameter(chg.id as i32, chg.value);
                        if let Some(ref r) = dsp.inst_r {
                            r.set_parameter(chg.id as i32, chg.value);
                        }
                    }

                    // 2. Process audio
                    let frames = (bridge.num_frames as usize).min(protocol::MAX_FRAMES);
                    if frames > 0 {
                        let (in_l_slice, in_r_slice, out_l_slice, out_r_slice) = unsafe {
                            let b_ptr = addr as *mut protocol::ZfxSharedBridge;
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
                            dsp.inst_l.process_audio(&[in_l_slice], &mut [out_l_slice], frames);
                            r.process_audio(&[in_r_slice], &mut [out_r_slice], frames);
                        } else {
                            dsp.inst_l.process_audio(&[in_l_slice, in_r_slice], &mut [out_l_slice, out_r_slice], frames);
                        }
                    }
                    bridge.status.store(protocol::STATUS_OK, Ordering::Release);
                } else {
                    bridge.status.store(protocol::STATUS_ERROR, Ordering::Release);
                }
                bridge.worker_seq.store(host_seq, Ordering::Release);
                unsafe {
                    protocol::futex_wake(&bridge.worker_seq);
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
                unsafe {
                    protocol::futex_wake(&bridge.worker_seq);
                }
                break;
            }

            _ => {
                bridge.worker_seq.store(host_seq, Ordering::Release);
                unsafe {
                    protocol::futex_wake(&bridge.worker_seq);
                }
            }
        }
    }
}

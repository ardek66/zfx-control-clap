use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;
use goblin::pe::PE;
use once_cell::sync::Lazy;

/// Represents our embedded, memory-mapped Windows DLL.
pub struct PeModule {
    pub base_addr: *mut c_void,
    pub size: usize,
}

impl PeModule {
    /// Loads a PE image from raw bytes into executable memory.
    pub fn load(raw_dll_bytes: &[u8]) -> Result<Self, &'static str> {
        let pe = PE::parse(raw_dll_bytes).map_err(|_| "Failed to parse PE")?;

        if pe.is_64 {
            return Err("Expected 32-bit PE");
        }

        let opt_hdr = pe.header.optional_header.ok_or("Missing Optional Header")?;
        let size_of_image = opt_hdr.windows_fields.size_of_image as usize;
        let image_base = opt_hdr.windows_fields.image_base as usize;

        if size_of_image == 0 {
            return Err("Invalid SizeOfImage");
        }

        // 1. Allocate executable memory for the full image
        let base_addr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                size_of_image,
                libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            )
        };

        if base_addr == libc::MAP_FAILED {
            return Err("Failed to allocate executable memory via mmap");
        }

        let base_ptr = base_addr as *mut u8;

        // 2. Copy PE headers
        let size_of_headers = opt_hdr.windows_fields.size_of_headers as usize;
        unsafe {
            ptr::copy_nonoverlapping(raw_dll_bytes.as_ptr(), base_ptr, size_of_headers.min(raw_dll_bytes.len()));
        }

        // 3. Copy PE sections to their virtual addresses
        for section in &pe.sections {
            if section.size_of_raw_data > 0 {
                let raw_start = section.pointer_to_raw_data as usize;
                let raw_size = section.size_of_raw_data as usize;
                let virt_start = section.virtual_address as usize;
                if raw_start + raw_size <= raw_dll_bytes.len() && virt_start + raw_size <= size_of_image {
                    unsafe {
                        ptr::copy_nonoverlapping(
                            raw_dll_bytes.as_ptr().add(raw_start),
                            base_ptr.add(virt_start),
                            raw_size,
                        );
                    }
                }
            }
        }

        // 4. Apply Base Relocations if the image was not loaded at image_base
        let delta = (base_ptr as usize as isize).wrapping_sub(image_base as isize);
        eprintln!("[peldr] base_ptr={:?}, image_base={:#x}, delta={:#x}", base_ptr, image_base, delta);
        if delta != 0 {
            unsafe {
                Self::apply_relocations(base_ptr, raw_dll_bytes, delta)?;
            }
        }
        eprintln!("[peldr] Relocations applied successfully");

        // 5. Resolve Imports into the IAT
        unsafe {
            Self::resolve_imports(base_ptr, raw_dll_bytes)?;
        }
        eprintln!("[peldr] Imports resolved successfully");

        // 6. Automatically patch hardware dongle check if present
        // Signature: sub esp, 0x44c -> [0x81, 0xec, 0x4c, 0x04, 0x00, 0x00]
        // Replacement: mov al, 1; ret; nop; nop; nop -> [0xb0, 0x01, 0xc3, 0x90, 0x90, 0x90]
        // Note: The function is __cdecl, so caller cleans up the arguments with `add esp, 0x18`.
        // A `ret` (0xc3) preserves stack balance, whereas `ret 8` corrupted the caller's stack frame!
        let sig = [0x81u8, 0xec, 0x4c, 0x04, 0x00, 0x00];
        let patch = [0xb0u8, 0x01, 0xc3, 0x90, 0x90, 0x90];
        let mem_slice = unsafe { std::slice::from_raw_parts_mut(base_ptr, size_of_image) };
        if let Some(pos) = mem_slice.windows(sig.len()).position(|w| w == sig) {
            mem_slice[pos..pos + patch.len()].copy_from_slice(&patch);
            eprintln!("[peldr] Hardware dongle check bypassed at RVA {:#x}", pos);
        }

        Ok(Self { base_addr, size: size_of_image })
    }

    /// Applies base relocations from the .reloc directory
    unsafe fn apply_relocations(base_ptr: *mut u8, raw_dll: &[u8], delta: isize) -> Result<(), &'static str> {
        let pe_off = u32::from_le_bytes(raw_dll[0x3c..0x40].try_into().unwrap()) as usize;
        let opt_off = pe_off + 24;
        // DataDirectory[5] is Base Relocation Table (offset 136 in optional header)
        if opt_off + 144 > raw_dll.len() {
            return Ok(());
        }
        let reloc_rva = u32::from_le_bytes(raw_dll[opt_off + 136..opt_off + 140].try_into().unwrap()) as usize;
        let reloc_size = u32::from_le_bytes(raw_dll[opt_off + 140..opt_off + 144].try_into().unwrap()) as usize;

        if reloc_rva == 0 || reloc_size == 0 {
            return Ok(());
        }

        let mut cur = 0;
        while cur < reloc_size {
            let block_ptr = base_ptr.add(reloc_rva + cur);
            let page_rva = ptr::read_unaligned(block_ptr as *const u32) as usize;
            let block_size = ptr::read_unaligned(block_ptr.add(4) as *const u32) as usize;

            if block_size == 0 || block_size < 8 {
                break;
            }

            let num_entries = (block_size - 8) / 2;
            for i in 0..num_entries {
                let entry = ptr::read_unaligned(block_ptr.add(8 + i * 2) as *const u16);
                let rtype = entry >> 12;
                let roffset = (entry & 0x0FFF) as usize;

                if rtype == 3 {
                    // IMAGE_REL_BASED_HIGHLOW (32-bit delta adjustment)
                    let patch_addr = base_ptr.add(page_rva + roffset) as *mut u32;
                    let old_val = ptr::read_unaligned(patch_addr);
                    let new_val = old_val.wrapping_add(delta as u32);
                    ptr::write_unaligned(patch_addr, new_val);
                }
            }
            cur += block_size;
        }

        Ok(())
    }

    /// Resolves external Win32 / CRT functions into the Import Address Table (IAT)
    unsafe fn resolve_imports(base_ptr: *mut u8, raw_dll: &[u8]) -> Result<(), &'static str> {
        let pe_off = u32::from_le_bytes(raw_dll[0x3c..0x40].try_into().unwrap()) as usize;
        let opt_off = pe_off + 24;
        // DataDirectory[1] is Import Table (offset 104 in optional header)
        if opt_off + 112 > raw_dll.len() {
            return Ok(());
        }
        let imp_rva = u32::from_le_bytes(raw_dll[opt_off + 104..opt_off + 108].try_into().unwrap()) as usize;
        let imp_size = u32::from_le_bytes(raw_dll[opt_off + 108..opt_off + 112].try_into().unwrap()) as usize;

        if imp_rva == 0 || imp_size == 0 {
            return Ok(());
        }

        let mut desc_idx = 0;
        loop {
            let desc_ptr = base_ptr.add(imp_rva + desc_idx * 20);
            let oft = ptr::read_unaligned(desc_ptr as *const u32) as usize;
            let name_rva = ptr::read_unaligned(desc_ptr.add(12) as *const u32) as usize;
            let ft = ptr::read_unaligned(desc_ptr.add(16) as *const u32) as usize;

            if name_rva == 0 && ft == 0 {
                break; // End of import descriptors
            }

            let dll_name_str = CStr::from_ptr(base_ptr.add(name_rva) as *const i8)
                .to_string_lossy()
                .to_lowercase();

            let thunk_rva = if oft != 0 { oft } else { ft };
            let iat_rva = ft;

            let mut slot_idx = 0;
            loop {
                let thunk_entry = ptr::read_unaligned(base_ptr.add(thunk_rva + slot_idx * 4) as *const u32);
                if thunk_entry == 0 {
                    break;
                }

                let target_iat = base_ptr.add(iat_rva + slot_idx * 4) as *mut *mut c_void;

                if (thunk_entry & 0x80000000) == 0 {
                    // Import by name
                    let name_entry_ptr = base_ptr.add(thunk_entry as usize);
                    let func_name = CStr::from_ptr(name_entry_ptr.add(2) as *const i8)
                        .to_string_lossy();

                    let hook_ptr = resolve_import_symbol(&dll_name_str, &func_name);
                    eprintln!("[peldr import] {}!{} -> {:?}", dll_name_str, func_name, hook_ptr);
                    *target_iat = hook_ptr;
                }

                slot_idx += 1;
            }

            desc_idx += 1;
        }

        Ok(())
    }

    /// Resolves an exported function by name from the EAT (Export Address Table)
    pub fn get_export(&self, name: &str) -> Option<*mut c_void> {
        let base_ptr = self.base_addr as *mut u8;
        unsafe {
            let pe_off = ptr::read_unaligned(base_ptr.add(0x3c) as *const u32) as usize;
            let opt_off = pe_off + 24;
            let exp_rva = ptr::read_unaligned(base_ptr.add(opt_off + 96) as *const u32) as usize;
            if exp_rva == 0 {
                return None;
            }

            let exp_ptr = base_ptr.add(exp_rva);
            let num_functions = ptr::read_unaligned(exp_ptr.add(20) as *const u32) as usize;
            let num_names = ptr::read_unaligned(exp_ptr.add(24) as *const u32) as usize;
            let funcs_rva = ptr::read_unaligned(exp_ptr.add(28) as *const u32) as usize;
            let names_rva = ptr::read_unaligned(exp_ptr.add(32) as *const u32) as usize;
            let ords_rva = ptr::read_unaligned(exp_ptr.add(36) as *const u32) as usize;

            for i in 0..num_names {
                let n_rva = ptr::read_unaligned(base_ptr.add(names_rva + i * 4) as *const u32) as usize;
                let sym_name = CStr::from_ptr(base_ptr.add(n_rva) as *const i8).to_string_lossy();
                if sym_name == name {
                    let ord = ptr::read_unaligned(base_ptr.add(ords_rva + i * 2) as *const u16) as usize;
                    if ord < num_functions {
                        let fn_rva = ptr::read_unaligned(base_ptr.add(funcs_rva + ord * 4) as *const u32) as usize;
                        return Some(base_ptr.add(fn_rva) as *mut c_void);
                    }
                }
            }
        }
        None
    }

    /// Resolves the ZTypeIF export
    pub fn get_ztypeif(&self) -> Result<extern "C" fn(), &'static str> {
        let export_fn = self.get_export("ZTypeIF")
            .or_else(|| self.get_export("getinterface"))
            .ok_or("Failed to locate ZTypeIF export")?;
        Ok(unsafe { std::mem::transmute(export_fn) })
    }

    /// Resolves ZTypeIF or getinterface and initializes the DSP instance with host callback
    pub fn init_zfx_plugin(&self) -> Result<ZfxDspInstance, &'static str> {
        let export_fn = self.get_export("ZTypeIF")
            .or_else(|| self.get_export("getinterface"))
            .ok_or("Failed to locate ZTypeIF or getinterface export in PE")?;

        eprintln!("[peldr] Located getinterface export at {:?}", export_fn);

        // Ensure Win32 TEB and FS segment register are initialized
        init_win32_teb();

        let getinterface: extern "C" fn(host_callback: *const c_void) -> *mut c_void =
            unsafe { std::mem::transmute(export_fn) };

        eprintln!("[peldr] Calling getinterface({:?})...", default_audio_master as *const c_void);
        let instance = unsafe {
            let _guard = Win32ContextGuard::enter();
            getinterface(default_audio_master as *const c_void)
        };
        eprintln!("[peldr] Returned from getinterface: instance={:?}", instance);
        if instance.is_null() {
            return Err("getinterface returned null pointer");
        }

        let iface = instance as *mut *mut c_void;
        let effect = unsafe { *iface.add(1) as *mut AEffect };
        if effect.is_null() {
            return Err("AEffect pointer is null");
        }
        let magic = unsafe { (*effect).magic };
        if magic != 0x56737450 {
            return Err("AEffect magic does not match 'VstP'");
        }

        Ok(ZfxDspInstance {
            iface_wrapper: instance,
            effect,
        })
    }
}

#[repr(C)]
pub struct AEffect {
    pub magic: i32, // 'VstP' (0x56737450)
    pub dispatcher: unsafe extern "C" fn(
        effect: *mut AEffect,
        opcode: i32,
        index: i32,
        value: isize,
        ptr: *mut c_void,
        opt: f32,
    ) -> isize,
    pub process: unsafe extern "C" fn(
        effect: *mut AEffect,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        sampleframes: i32,
    ),
    pub set_parameter: unsafe extern "C" fn(
        effect: *mut AEffect,
        index: i32,
        parameter: f32,
    ),
    pub get_parameter: unsafe extern "C" fn(
        effect: *mut AEffect,
        index: i32,
    ) -> f32,
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
        effect: *mut AEffect,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        sampleframes: i32,
    ),
    pub process_double_replacing: *mut c_void,
}

#[derive(Clone, Copy)]
pub struct ZfxDspInstance {
    pub iface_wrapper: *mut c_void,
    pub effect: *mut AEffect,
}

unsafe impl Send for ZfxDspInstance {}
unsafe impl Sync for ZfxDspInstance {}

impl ZfxDspInstance {
    pub fn open(&self) {
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 0 = effOpen in VST 2.4
            ((*self.effect).dispatcher)(self.effect, 0, 0, 0, ptr::null_mut(), 0.0);
        }
    }

    pub fn close(&self) {
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 1 = effClose in VST 2.4
            ((*self.effect).dispatcher)(self.effect, 1, 0, 0, ptr::null_mut(), 0.0);
        }
    }

    pub fn num_params(&self) -> i32 {
        unsafe { (*self.effect).num_params }
    }

    pub fn num_inputs(&self) -> i32 {
        unsafe { (*self.effect).num_inputs }
    }

    pub fn num_outputs(&self) -> i32 {
        unsafe { (*self.effect).num_outputs }
    }

    pub fn get_parameter_name(&self, index: i32) -> String {
        let mut buf = [0u8; 32];
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 8 = effGetParamName
            ((*self.effect).dispatcher)(self.effect, 8, index, 0, buf.as_mut_ptr() as *mut c_void, 0.0);
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len]).trim().to_string()
    }

    pub fn set_parameter(&self, index: i32, value: f32) {
        unsafe {
            let _guard = Win32ContextGuard::enter();
            ((*self.effect).set_parameter)(self.effect, index, value);
        }
    }

    pub fn get_parameter(&self, index: i32) -> f32 {
        unsafe {
            let _guard = Win32ContextGuard::enter();
            ((*self.effect).get_parameter)(self.effect, index)
        }
    }

    pub fn process_replacing_f64(&self, inputs: *const *const f64, outputs: *mut *mut f64, sample_frames: i32) {
        unsafe {
            let obj = (*self.effect).object as *mut *mut usize;
            let vtable = *obj as *mut usize;
            let dsp_proc_fn = *vtable.add(9);
            let _guard = Win32ContextGuard::enter();
            std::arch::asm!(
                "push {frames}",
                "push {outs}",
                "push {ins}",
                "call {proc}",
                frames = in(reg) sample_frames,
                outs = in(reg) outputs,
                ins = in(reg) inputs,
                proc = in(reg) dsp_proc_fn,
                in("ecx") obj,
                clobber_abi("C"),
            );
        }
    }

    pub fn process_audio(&self, inputs: &[&[f32]], outputs: &mut [&mut [f32]], frames: usize) {
        if frames == 0 {
            return;
        }
        // VST DSP engines in Zoom ZFX (including Analog_Delay) unconditionally dereference
        // at least 2 input channel pointers and 2 output channel pointers!
        let dsp_ins = unsafe { (*self.effect).num_inputs.max(2) as usize };
        let dsp_outs = unsafe { (*self.effect).num_outputs.max(2) as usize };

        // Prepare 64-bit input buffers
        let in_f64: Vec<Vec<f64>> = (0..dsp_ins).map(|ch| {
            if inputs.is_empty() {
                vec![0.0f64; frames]
            } else if ch < inputs.len() {
                inputs[ch].iter().map(|&s| s as f64).collect()
            } else {
                // If DSP requires channel 1 but host only provided channel 0, duplicate channel 0
                inputs[0].iter().map(|&s| s as f64).collect()
            }
        }).collect();

        let mut out_f64: Vec<Vec<f64>> = (0..dsp_outs).map(|_| vec![0.0f64; frames]).collect();

        let in_ptrs: Vec<*const f64> = in_f64.iter().map(|b| b.as_ptr()).collect();
        let mut out_ptrs: Vec<*mut f64> = out_f64.iter_mut().map(|b| b.as_mut_ptr()).collect();

        self.process_replacing_f64(in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), frames as i32);

        // Copy back to host outputs
        for (ch, out_buf) in outputs.iter_mut().enumerate() {
            let src_ch = if ch < dsp_outs { ch } else { 0 };
            if src_ch < out_f64.len() {
                for (s_out, &s_in) in out_buf.iter_mut().zip(out_f64[src_ch].iter()) {
                    *s_out = if s_in.is_finite() {
                        (s_in as f32).clamp(-10.0, 10.0)
                    } else {
                        0.0
                    };
                }
            }
        }
    }

    pub fn process_replacing(&self, inputs: *const *const f32, outputs: *mut *mut f32, sample_frames: i32) {
        unsafe {
            let _guard = Win32ContextGuard::enter();
            ((*self.effect).process_replacing)(self.effect, inputs, outputs, sample_frames);
        }
    }

    pub fn set_sample_rate(&self, sample_rate: f32) {
        CURRENT_SAMPLE_RATE.store(sample_rate as i32, Ordering::Relaxed);
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 10 = effSetSampleRate in VST 2.4
            ((*self.effect).dispatcher)(self.effect, 10, 0, 0, ptr::null_mut(), sample_rate);
        }
    }

    pub fn set_block_size(&self, block_size: i32) {
        CURRENT_BLOCK_SIZE.store(block_size, Ordering::Relaxed);
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 11 = effSetBlockSize in VST 2.4
            ((*self.effect).dispatcher)(self.effect, 11, 0, block_size as isize, ptr::null_mut(), 0.0);
        }
    }

    pub fn resume(&self) {
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 12 = effMainsChanged (1 = resume / start processing)
            ((*self.effect).dispatcher)(self.effect, 12, 0, 1, ptr::null_mut(), 0.0);
        }
    }

    pub fn suspend(&self) {
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 12 = effMainsChanged (0 = suspend / stop processing)
            ((*self.effect).dispatcher)(self.effect, 12, 0, 0, ptr::null_mut(), 0.0);
        }
    }
}

impl Drop for PeModule {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base_addr, self.size);
        }
    }
}

pub static CURRENT_SAMPLE_RATE: AtomicI32 = AtomicI32::new(44100);
pub static CURRENT_BLOCK_SIZE: AtomicI32 = AtomicI32::new(256);

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
    // kVstTransportPlaying (2) | kVstTempoValid (1024) | kVstPpqPosValid (512) | kVstTimeSigValid (8192)
    flags: 0x0002 | 0x0400 | 0x0200 | 0x2000,
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
    eprintln!("[default_audio_master] opcode={}, index={}, value={}, opt={}", opcode, index, value, opt);
    match opcode {
        1 => 2400, // audioMasterVersion (VST 2.4)
        7 => {
            // audioMasterGetTime (VST 2.4) - requested by delays, tap-tempo, beat-sync effects
            CURRENT_TIME_INFO.sample_rate = CURRENT_SAMPLE_RATE.load(Ordering::Relaxed) as f64;
            &raw const CURRENT_TIME_INFO as isize
        }
        16 => CURRENT_SAMPLE_RATE.load(Ordering::Relaxed) as isize, // audioMasterGetSampleRate
        17 => CURRENT_BLOCK_SIZE.load(Ordering::Relaxed) as isize,  // audioMasterGetBlockSize
        _ => 0, // In VST 2.4, unknown/unsupported opcodes MUST return 0 (NULL/false), NEVER 1!
    }
}

// --- Import Symbol Dispatcher ---

fn resolve_import_symbol(dll: &str, func: &str) -> *mut c_void {
    match (dll, func) {
        // SETUPAPI.dll (Hardware Dongle Protection Spoofing)
        ("setupapi.dll", "SetupDiGetClassDevsA") => hook_SetupDiGetClassDevsA as *mut c_void,
        ("setupapi.dll", "SetupDiEnumDeviceInterfaces") => hook_SetupDiEnumDeviceInterfaces as *mut c_void,
        ("setupapi.dll", "SetupDiGetDeviceInterfaceDetailA") => hook_SetupDiGetDeviceInterfaceDetailA as *mut c_void,
        ("setupapi.dll", "SetupDiGetDeviceRegistryPropertyA") => hook_SetupDiGetDeviceRegistryPropertyA as *mut c_void,
        ("setupapi.dll", "SetupDiDestroyDeviceInfoList") => hook_SetupDiDestroyDeviceInfoList as *mut c_void,

        // KERNEL32.dll
        ("kernel32.dll", "InitializeCriticalSection") => hook_InitializeCriticalSection as *mut c_void,
        ("kernel32.dll", "DeleteCriticalSection") => hook_DeleteCriticalSection as *mut c_void,
        ("kernel32.dll", "EnterCriticalSection") => hook_EnterCriticalSection as *mut c_void,
        ("kernel32.dll", "LeaveCriticalSection") => hook_LeaveCriticalSection as *mut c_void,
        ("kernel32.dll", "GetCurrentProcess") => hook_GetCurrentProcess as *mut c_void,
        ("kernel32.dll", "GetCurrentProcessId") => hook_GetCurrentProcessId as *mut c_void,
        ("kernel32.dll", "GetCurrentThreadId") => hook_GetCurrentThreadId as *mut c_void,
        ("kernel32.dll", "GetLastError") => hook_GetLastError as *mut c_void,
        ("kernel32.dll", "GetTickCount") => hook_GetTickCount as *mut c_void,
        ("kernel32.dll", "GetSystemTimeAsFileTime") => hook_GetSystemTimeAsFileTime as *mut c_void,
        ("kernel32.dll", "QueryPerformanceCounter") => hook_QueryPerformanceCounter as *mut c_void,
        ("kernel32.dll", "InterlockedCompareExchange") => hook_InterlockedCompareExchange as *mut c_void,
        ("kernel32.dll", "InterlockedExchange") => hook_InterlockedExchange as *mut c_void,
        ("kernel32.dll", "IsDebuggerPresent") => hook_IsDebuggerPresent as *mut c_void,
        ("kernel32.dll", "Sleep") => hook_Sleep as *mut c_void,
        ("kernel32.dll", "SetUnhandledExceptionFilter") => hook_SetUnhandledExceptionFilter as *mut c_void,
        ("kernel32.dll", "UnhandledExceptionFilter") => hook_UnhandledExceptionFilter as *mut c_void,
        ("kernel32.dll", "TerminateProcess") => hook_TerminateProcess as *mut c_void,

        // MSVCR80.dll (Memory, String, and Math)
        ("msvcr80.dll", "_malloc_crt") | ("msvcr80.dll", "??2@YAPAXI@Z") | ("msvcr80.dll", "??_U@YAPAXI@Z") => {
            hook_malloc as *mut c_void
        }
        ("msvcr80.dll", "free") | ("msvcr80.dll", "??3@YAXPAX@Z") | ("msvcr80.dll", "??_V@YAXPAX@Z") => {
            hook_free as *mut c_void
        }
        ("msvcr80.dll", "memcpy") => libc::memcpy as *mut c_void,
        ("msvcr80.dll", "memset") => libc::memset as *mut c_void,
        ("msvcr80.dll", "strncpy") => libc::strncpy as *mut c_void,
        ("msvcr80.dll", "strncat") => libc::strncat as *mut c_void,
        ("msvcr80.dll", "strtol") => libc::strtol as *mut c_void,
        ("msvcr80.dll", "toupper") => libc::toupper as *mut c_void,
        ("msvcr80.dll", "strcpy_s") => hook_strcpy_s as *mut c_void,
        ("msvcr80.dll", "vsprintf") => libc::sprintf as *mut c_void,

        // Math
        ("msvcr80.dll", "_CIfmod") => hook_cifmod as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_atan") => hook_atan as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_cos") => hook_cos as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_exp") => hook_exp as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_log") => hook_log as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_log10") => hook_log10 as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_pow") => hook_pow as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_sin") => hook_sin as *mut c_void,
        ("msvcr80.dll", "__libm_sse2_tan") => hook_tan as *mut c_void,

        // CRT internals
        ("msvcr80.dll", "_initterm") | ("msvcr80.dll", "_initterm_e") => hook_initterm as *mut c_void,
        ("msvcr80.dll", "_encode_pointer") | ("msvcr80.dll", "_decode_pointer") => hook_identity as *mut c_void,
        ("msvcr80.dll", "_encoded_null") => hook_null as *mut c_void,
        ("msvcr80.dll", "_lock") | ("msvcr80.dll", "_unlock") | ("msvcr80.dll", "_amsg_exit") => hook_noop as *mut c_void,
        ("msvcr80.dll", "_crt_debugger_hook") => hook_noop as *mut c_void,
        ("msvcr80.dll", "_onexit") => hook_onexit as *mut c_void,
        ("msvcr80.dll", "__dllonexit") => hook_dllonexit as *mut c_void,
        ("msvcr80.dll", "_purecall") => hook_purecall as *mut c_void,
        ("msvcr80.dll", "?terminate@@YAXXZ") => hook_noop as *mut c_void,
        ("msvcr80.dll", "__CppXcptFilter") | ("msvcr80.dll", "_except_handler4_common") | ("msvcr80.dll", "__CxxFrameHandler3") => {
            hook_zero as *mut c_void
        }
        ("msvcr80.dll", "?_type_info_dtor_internal_method@type_info@@QAEXXZ") => hook_noop as *mut c_void,
        ("msvcr80.dll", "__clean_type_info_names_internal") => hook_noop as *mut c_void,
        ("msvcr80.dll", "__RTDynamicCast") => hook_identity as *mut c_void,
        ("msvcr80.dll", "_adjust_fdiv") => &raw mut ADJUST_FDIV as *mut c_void,

        _ => {
            eprintln!("[peldr] Warning: Unhandled import {}!{}", dll, func);
            hook_noop as *mut c_void
        }
    }
}

// --- Win32 TEB and Thread Area Setup ---
#[repr(C)]
struct UserDesc {
    entry_number: u32,
    base_addr: u32,
    limit: u32,
    flags: u32,
}

static mut WIN32_TEB: [u32; 1024] = [0; 1024];
static mut TEB_SELECTOR: u16 = 0;

pub fn init_win32_teb() {
    unsafe {
        if TEB_SELECTOR != 0 {
            return;
        }

        WIN32_TEB[0] = 0xFFFFFFFF; // Head of SEH chain
        WIN32_TEB[6] = &raw const WIN32_TEB as u32; // Self pointer at TEB+0x18
        WIN32_TEB[8] = libc::getpid() as u32;     // Process ID at TEB+0x20
        WIN32_TEB[9] = libc::pthread_self() as u32; // Thread ID at TEB+0x24

        let mut desc = UserDesc {
            entry_number: 0xFFFFFFFF,
            base_addr: &raw const WIN32_TEB as u32,
            limit: 4096,
            flags: 1 | (1 << 6), // 32-bit segment + useable
        };

        // Syscall 243 is SYS_set_thread_area on i386
        let res = libc::syscall(243, &mut desc);
        if res == 0 {
            let sel = ((desc.entry_number << 3) | 3) as u16;
            TEB_SELECTOR = sel;
            eprintln!("[peldr] Win32 TEB initialized successfully! Selector={:#x}", sel);
        } else {
            eprintln!("[peldr] Warning: SYS_set_thread_area returned {}", res);
        }
    }
}

pub struct Win32ContextGuard {
    saved_fs: u16,
}

impl Win32ContextGuard {
    #[inline(always)]
    pub unsafe fn enter() -> Self {
        let mut saved_fs: u16 = 0;
        let sel = TEB_SELECTOR;
        if sel != 0 {
            std::arch::asm!(
                "mov {saved:x}, fs",
                "mov fs, {sel:x}",
                saved = out(reg) saved_fs,
                sel = in(reg) sel,
                options(nostack, preserves_flags)
            );
        }
        Self { saved_fs }
    }
}

impl Drop for Win32ContextGuard {
    #[inline(always)]
    fn drop(&mut self) {
        if self.saved_fs != 0 {
            unsafe {
                std::arch::asm!(
                    "mov fs, {saved:x}",
                    saved = in(reg) self.saved_fs,
                    options(nostack, preserves_flags)
                );
            }
        }
    }
}

// --- Global CRT variables ---
#[no_mangle]
pub static mut ADJUST_FDIV: i32 = 0;

static START_TIME: Lazy<Instant> = Lazy::new(Instant::now);

// --- SETUPAPI Trampolines (Hardware Dongle Spoofing) ---

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiGetClassDevsA(
    _class_guid: *const c_void,
    _enumerator: *const c_void,
    _hwnd_parent: *mut c_void,
    _flags: u32,
) -> *mut c_void {
    eprintln!("[hook_SetupDiGetClassDevsA]");
    0x13370001 as *mut c_void
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiEnumDeviceInterfaces(
    _device_info_set: *mut c_void,
    _device_info_data: *mut c_void,
    _interface_class_guid: *const c_void,
    member_index: u32,
    device_interface_data: *mut c_void,
) -> i32 {
    eprintln!("[hook_SetupDiEnumDeviceInterfaces] member_index={}", member_index);
    if member_index == 0 {
        if !device_interface_data.is_null() {
            // SP_DEVICE_INTERFACE_DATA: cbSize is at offset 0, Flags is at offset 20
            let p = device_interface_data as *mut u32;
            *p = 28; // sizeof(SP_DEVICE_INTERFACE_DATA)
            *p.add(5) = 1; // SPINT_ACTIVE
        }
        1 // TRUE: Zoom hardware device found!
    } else {
        0 // FALSE: No more items
    }
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiGetDeviceInterfaceDetailA(
    _device_info_set: *mut c_void,
    _device_interface_data: *mut c_void,
    device_interface_detail_data: *mut c_void,
    device_interface_detail_data_size: u32,
    required_size: *mut u32,
    _device_info_data: *mut c_void,
) -> i32 {
    const SPOOFED_PATH: &[u8] = b"\\\\?\\usb#vid_1686&pid_0047#c51t#{a5dcbf10-11d2-6530-c000-1f90ed51b94f}\0";
    let needed = (4 + SPOOFED_PATH.len()) as u32;

    if !required_size.is_null() {
        *required_size = needed;
    }

    if !device_interface_detail_data.is_null() && device_interface_detail_data_size >= needed {
        // cbSize = 5 for SP_DEVICE_INTERFACE_DETAIL_DATA_A on 32-bit Windows
        let p = device_interface_detail_data as *mut u8;
        *(p as *mut u32) = 5;
        ptr::copy_nonoverlapping(SPOOFED_PATH.as_ptr(), p.add(4), SPOOFED_PATH.len());
    }

    eprintln!("[hook_SetupDiGetDeviceInterfaceDetailA] size={}, needed={}", device_interface_detail_data_size, needed);
    1 // TRUE
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiGetDeviceRegistryPropertyA(
    _device_info_set: *mut c_void,
    _device_info_data: *mut c_void,
    property: u32,
    property_reg_data_type: *mut u32,
    property_buffer: *mut u8,
    property_buffer_size: u32,
    required_size: *mut u32,
) -> i32 {
    const HARDWARE_ID: &[u8] = b"USB\\VID_1686&PID_0047\0\0";
    eprintln!("[hook_SetupDiGetDeviceRegistryPropertyA] prop={}, buf_size={}", property, property_buffer_size);
    if !required_size.is_null() {
        *required_size = HARDWARE_ID.len() as u32;
    }
    if !property_reg_data_type.is_null() {
        *property_reg_data_type = 1; // REG_SZ
    }
    if !property_buffer.is_null() && property_buffer_size >= HARDWARE_ID.len() as u32 {
        ptr::copy_nonoverlapping(HARDWARE_ID.as_ptr(), property_buffer, HARDWARE_ID.len());
    }
    1
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiDestroyDeviceInfoList(_device_info_set: *mut c_void) -> i32 {
    eprintln!("[hook_SetupDiDestroyDeviceInfoList]");
    1
}

// --- KERNEL32 Trampolines ---

#[no_mangle]
pub unsafe extern "stdcall" fn hook_InitializeCriticalSection(cs: *mut c_void) {
    if !cs.is_null() {
        ptr::write_bytes(cs as *mut u8, 0, 24);
    }
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_DeleteCriticalSection(_cs: *mut c_void) {}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_EnterCriticalSection(_cs: *mut c_void) {}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_LeaveCriticalSection(_cs: *mut c_void) {}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetCurrentProcess() -> *mut c_void {
    -1isize as *mut c_void
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetCurrentProcessId() -> u32 {
    libc::getpid() as u32
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetCurrentThreadId() -> u32 {
    libc::pthread_self() as u32
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetLastError() -> u32 {
    0
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetTickCount() -> u32 {
    START_TIME.elapsed().as_millis() as u32
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetSystemTimeAsFileTime(ft: *mut u64) {
    if !ft.is_null() {
        *ft = 130000000000000000;
    }
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_QueryPerformanceCounter(pc: *mut i64) -> i32 {
    if !pc.is_null() {
        *pc = START_TIME.elapsed().as_nanos() as i64;
    }
    1
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_InterlockedCompareExchange(dest: *mut i32, exchange: i32, comparand: i32) -> i32 {
    if dest.is_null() {
        return 0;
    }
    let atomic = &*(dest as *const AtomicI32);
    atomic.compare_exchange(comparand, exchange, Ordering::SeqCst, Ordering::SeqCst).unwrap_or_else(|v| v)
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_InterlockedExchange(target: *mut i32, val: i32) -> i32 {
    if target.is_null() {
        return 0;
    }
    let atomic = &*(target as *const AtomicI32);
    atomic.swap(val, Ordering::SeqCst)
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_IsDebuggerPresent() -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_Sleep(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetUnhandledExceptionFilter(_f: *mut c_void) -> *mut c_void {
    ptr::null_mut()
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_UnhandledExceptionFilter(_p: *mut c_void) -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_TerminateProcess(_h: *mut c_void, _code: u32) -> i32 {
    1
}

// --- MSVCR80 CRT Trampolines ---

#[no_mangle]
pub unsafe extern "C" fn hook_malloc(size: usize) -> *mut c_void {
    let p = libc::malloc(size);
    eprintln!("[hook_malloc] size={} -> {:?}", size, p);
    p
}

#[no_mangle]
pub unsafe extern "C" fn hook_free(ptr: *mut c_void) {
    libc::free(ptr);
}

#[no_mangle]
pub unsafe extern "C" fn hook_strcpy_s(dst: *mut i8, size: usize, src: *const i8) -> i32 {
    if dst.is_null() || src.is_null() || size == 0 {
        return 22; // EINVAL
    }
    let len = libc::strlen(src);
    if len >= size {
        *dst = 0;
        return 34; // ERANGE
    }
    ptr::copy_nonoverlapping(src, dst, len + 1);
    0
}

#[no_mangle]
pub unsafe extern "C" fn hook_cifmod() {
    // In x86 cdecl / fastcall FPU convention: fmod(st(1), st(0))
    // On x86 FPU: fprem
    std::arch::asm!(
        "fprem",
        "fstsw ax",
        "sahf",
        "jpe 2f",
        "2:",
        options(nostack)
    );
}

extern "C" fn math_atan(x: f64) -> f64 { x.atan() }
extern "C" fn math_cos(x: f64) -> f64 { x.cos() }
extern "C" fn math_sin(x: f64) -> f64 { x.sin() }
extern "C" fn math_exp(x: f64) -> f64 { x.exp() }
extern "C" fn math_log(x: f64) -> f64 { x.ln() }
extern "C" fn math_log10(x: f64) -> f64 { x.log10() }
extern "C" fn math_pow(x: f64, y: f64) -> f64 { x.powf(y) }
extern "C" fn math_tan(x: f64) -> f64 { x.tan() }

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_atan() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "call {atan_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        atan_fn = sym math_atan,
    );
}

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_cos() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "call {cos_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        cos_fn = sym math_cos,
    );
}

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_sin() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "call {sin_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        sin_fn = sym math_sin,
    );
}

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_exp() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "call {exp_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        exp_fn = sym math_exp,
    );
}

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_log() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "call {log_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        log_fn = sym math_log,
    );
}

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_log10() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "call {log10_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        log10_fn = sym math_log10,
    );
}

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_pow() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "movsd [esp + 8], xmm1",
        "call {pow_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        pow_fn = sym math_pow,
    );
}

#[unsafe(naked)]
#[no_mangle]
pub unsafe extern "C" fn hook_tan() {
    std::arch::naked_asm!(
        "push ebp",
        "mov ebp, esp",
        "push ebx",
        "push esi",
        "push edi",
        "and esp, -16",
        "sub esp, 16",
        "movsd [esp], xmm0",
        "call {tan_fn}",
        "fstp qword ptr [esp]",
        "movsd xmm0, [esp]",
        "lea esp, [ebp - 12]",
        "pop edi",
        "pop esi",
        "pop ebx",
        "pop ebp",
        "ret",
        tan_fn = sym math_tan,
    );
}

#[no_mangle]
pub unsafe extern "C" fn hook_initterm(start: *const extern "C" fn(), end: *const extern "C" fn()) -> i32 {
    let mut cur = start;
    while cur < end {
        let f = *cur;
        if (f as usize) != 0 {
            f();
        }
        cur = cur.add(1);
    }
    0
}

#[no_mangle]
pub unsafe extern "C" fn hook_identity(p: *mut c_void) -> *mut c_void { p }

#[no_mangle]
pub unsafe extern "C" fn hook_null() -> *mut c_void { ptr::null_mut() }

#[no_mangle]
pub unsafe extern "C" fn hook_noop() {}

#[no_mangle]
pub unsafe extern "C" fn hook_zero() -> i32 { 0 }

#[no_mangle]
pub unsafe extern "C" fn hook_onexit(p: *mut c_void) -> *mut c_void { p }

#[no_mangle]
pub unsafe extern "C" fn hook_dllonexit(p: *mut c_void, _a: *mut c_void, _b: *mut c_void) -> *mut c_void { p }

#[no_mangle]
pub unsafe extern "C" fn hook_purecall() {
    eprintln!("[peldr] purecall called!");
}

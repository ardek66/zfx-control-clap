use goblin::pe::PE;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Instant;

use crate::analog_proto;
use crate::dsp_layout::{BiquadCoeffs, MemoryView, Order5Coeffs, WahResonatorParams};
use zerocopy::{little_endian as le, FromBytes, Immutable, IntoBytes, KnownLayout};

use std::sync::atomic::AtomicUsize;

const MAX_TRACKED_ALLOCS: usize = 1024;
static TRACKED_ALLOCS: [AtomicUsize; MAX_TRACKED_ALLOCS] =
    [const { AtomicUsize::new(0) }; MAX_TRACKED_ALLOCS];

#[inline]
pub fn is_plausible_heap_buf(ptr: usize) -> bool {
    if ptr == 0 {
        return false;
    }
    TRACKED_ALLOCS
        .iter()
        .any(|p| p.load(Ordering::Relaxed) == ptr)
}

#[repr(C)]
struct HpsPitchHelper {
    vtable: *const *const c_void,
    active: i32,
    pitch_index: i32,
}

const unsafe extern "stdcall" fn hps_pitch_calc(_pitch: f32) {
    // Pitch calculation callback for Harmonized Pitch Shifter
}

struct SyncPtrTable([*const c_void; 4]);
// SAFETY: SyncPtrTable wraps an immutable table of static function pointers and null entries.
unsafe impl Sync for SyncPtrTable {}

static HPS_VTABLE: SyncPtrTable = SyncPtrTable([
    std::ptr::null(),
    hps_pitch_calc as *const c_void,
    std::ptr::null(),
    std::ptr::null(),
]);

/// Win32 Base Relocation Block Header (8 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageBaseRelocationBlock {
    pub virtual_address: le::U32,
    pub size_of_block: le::U32,
}

/// Win32 Import Directory Descriptor (20 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageImportDescriptor {
    pub original_first_thunk: le::U32,
    pub time_date_stamp: le::U32,
    pub forwarder_chain: le::U32,
    pub name_rva: le::U32,
    pub first_thunk: le::U32,
}

/// Win32 Export Directory Table (40 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageExportDirectory {
    pub characteristics: le::U32,
    pub time_date_stamp: le::U32,
    pub major_version: le::U16,
    pub minor_version: le::U16,
    pub name_rva: le::U32,
    pub ordinal_base: le::U32,
    pub number_of_functions: le::U32,
    pub number_of_names: le::U32,
    pub address_of_functions: le::U32,
    pub address_of_names: le::U32,
    pub address_of_name_ordinals: le::U32,
}

const _: () = assert!(std::mem::size_of::<ImageBaseRelocationBlock>() == 8);
const _: () = assert!(std::mem::size_of::<ImageImportDescriptor>() == 20);
const _: () = assert!(std::mem::size_of::<ImageExportDirectory>() == 40);

/// Represents our embedded, memory-mapped Windows DLL.
pub struct PeModule {
    pub base_addr: *mut c_void,
    pub size: usize,
}

impl PeModule {
    /// Loads a PE image from raw bytes into executable memory.
    #[allow(clippy::too_many_lines)]
    pub fn load(raw_dll_bytes: &[u8]) -> Result<Self, &'static str> {
        let pe = PE::parse(raw_dll_bytes).map_err(|_| "Failed to parse PE")?;

        if pe.is_64 {
            return Err("Expected 32-bit PE");
        }

        let opt_hdr = pe.header.optional_header.ok_or("Missing Optional Header")?;
        let size_of_image = usize::try_from(opt_hdr.windows_fields.size_of_image)
            .map_err(|_| "SizeOfImage overflow")?;
        let image_base =
            usize::try_from(opt_hdr.windows_fields.image_base).map_err(|_| "ImageBase overflow")?;

        if size_of_image == 0 {
            return Err("Invalid SizeOfImage");
        }

        // 1. Allocate executable memory for the full image
        #[cfg(target_os = "linux")]
        let base_addr = {
            // SAFETY: Allocating private anonymous page-aligned memory for the PE image.
            let addr = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    size_of_image,
                    libc::PROT_READ | libc::PROT_WRITE | libc::PROT_EXEC,
                    libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                    -1,
                    0,
                )
            };
            if addr == libc::MAP_FAILED {
                return Err("Failed to allocate executable memory via mmap");
            }
            addr
        };

        #[cfg(target_os = "windows")]
        let base_addr = {
            use windows_sys::Win32::System::Memory::{
                VirtualAlloc, MEM_COMMIT, MEM_RESERVE, PAGE_EXECUTE_READWRITE,
            };
            // SAFETY: Allocating executable memory for the PE image on Windows.
            let addr = unsafe {
                VirtualAlloc(
                    ptr::null_mut(),
                    size_of_image,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_EXECUTE_READWRITE,
                )
            };
            if addr.is_null() {
                return Err("Failed to allocate executable memory via VirtualAlloc");
            }
            addr
        };

        let base_ptr = base_addr.cast::<u8>();

        // 2. Copy PE headers
        let size_of_headers = usize::try_from(opt_hdr.windows_fields.size_of_headers)
            .map_err(|_| "SizeOfHeaders overflow")?;
        // SAFETY: Copying PE headers to base_ptr within allocated size_of_image.
        unsafe {
            ptr::copy_nonoverlapping(
                raw_dll_bytes.as_ptr(),
                base_ptr,
                size_of_headers.min(raw_dll_bytes.len()),
            );
        }

        // 3. Copy PE sections to their virtual addresses
        for section in &pe.sections {
            if section.size_of_raw_data > 0 {
                let raw_start = usize::try_from(section.pointer_to_raw_data).unwrap_or(0);
                let raw_size = usize::try_from(section.size_of_raw_data).unwrap_or(0);
                let virt_start = usize::try_from(section.virtual_address).unwrap_or(0);
                if raw_start + raw_size <= raw_dll_bytes.len()
                    && virt_start + raw_size <= size_of_image
                {
                    // SAFETY: Copying PE section raw bytes into mapped section memory.
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
        let delta = base_ptr
            .addr()
            .cast_signed()
            .wrapping_sub(image_base.cast_signed());
        eprintln!("[peldr] base_ptr={base_ptr:?}, image_base={image_base:#x}, delta={delta:#x}");
        if delta != 0 {
            // SAFETY: Applying relocations across valid mapped PE section memory.
            unsafe {
                Self::apply_relocations(base_ptr, raw_dll_bytes, delta);
            }
        }
        eprintln!("[peldr] Relocations applied successfully");

        // 5. Resolve Imports into the IAT
        // SAFETY: Resolving imported Win32 and CRT functions into the mapped IAT.
        unsafe {
            Self::resolve_imports(base_ptr, raw_dll_bytes);
        }
        eprintln!("[peldr] Imports resolved successfully");

        // 6. Automatically patch hardware dongle check if present
        // Signature: sub esp, 0x44c -> [0x81, 0xec, 0x4c, 0x04, 0x00, 0x00]
        // Replacement: mov al, 1; ret; nop; nop; nop -> [0xb0, 0x01, 0xc3, 0x90, 0x90, 0x90]
        // Note: The function is __cdecl, so caller cleans up the arguments with `add esp, 0x18`.
        // A `ret` (0xc3) preserves stack balance, whereas `ret 8` corrupted the caller's stack frame!
        let sig = [0x81u8, 0xec, 0x4c, 0x04, 0x00, 0x00];
        let patch = [0xb0u8, 0x01, 0xc3, 0x90, 0x90, 0x90];
        // SAFETY: Slicing the full mapped PE image memory to locate and patch dongle routine.
        let mem_slice = unsafe { std::slice::from_raw_parts_mut(base_ptr, size_of_image) };
        if let Some(pos) = mem_slice.windows(sig.len()).position(|w| w == sig) {
            mem_slice[pos..pos + patch.len()].copy_from_slice(&patch);
            eprintln!("[peldr] Hardware dongle check bypassed at RVA {pos:#x}");
        }

        // 7. Bypass redundant O(N^2) inner-loop envelope recalculation in Pedal_Pitch
        // In Pedal_Pitch, when rocking the pedal, 0x10002010 updates the pitch ratio in real time.
        // Immediately following at 0x10003234, Zoom's legacy code calls `edx` (0x100051f0) which
        // re-runs a loop over (sample_frames - i) samples on EVERY sample, causing O(N^2) CPU explosion
        // at 96k/192k (524k to 2M iterations/block).
        // By replacing `mov eax, [ebx + 0xf4]` at 0x10003234 with `jmp 0x10003299; nop`,
        // `call 0x10002010` runs live on every sample the pedal moves, but the O(N^2) table recalculation
        // is bypassed, restoring smooth real-time pedal tracking at 0% CPU overhead.
        // Signature at 0x10003228: mov esi, ebx; movss [esp], xmm0; call 0x10002010; mov eax, [ebx + 0xf4]
        let pitch_pedal_sig = [
            0x8bu8, 0xf3, 0xf3, 0x0f, 0x11, 0x04, 0x24, 0xe8, 0xdc, 0xed, 0xff, 0xff, 0x8b, 0x83,
            0xf4, 0x00, 0x00, 0x00,
        ];
        let pitch_pedal_patch = [0xe9u8, 0x60, 0x00, 0x00, 0x00, 0x90];
        if let Some(pos) = mem_slice
            .windows(pitch_pedal_sig.len())
            .position(|w| w == pitch_pedal_sig)
        {
            mem_slice[pos + 12..pos + 12 + pitch_pedal_patch.len()]
                .copy_from_slice(&pitch_pedal_patch);
            eprintln!(
                "[peldr] Pedal_Pitch O(N^2) loop hot-patched at RVA {:#x} (live pitch tracking preserved)",
                pos + 12
            );
        }

        Ok(Self {
            base_addr,
            size: size_of_image,
        })
    }

    /// Applies base relocations from the .reloc directory
    // SAFETY: Relocation targets can be unaligned in raw PE sections; read_unaligned and write_unaligned are used.
    unsafe fn apply_relocations(base_ptr: *mut u8, raw_dll: &[u8], delta: isize) {
        let pe_off =
            usize::try_from(u32::from_le_bytes(raw_dll[0x3c..0x40].try_into().unwrap())).unwrap();
        let opt_off = pe_off + 24;
        // DataDirectory[5] is Base Relocation Table (offset 136 in optional header)
        if opt_off + 144 > raw_dll.len() {
            return;
        }
        let reloc_rva = usize::try_from(u32::from_le_bytes(
            raw_dll[opt_off + 136..opt_off + 140].try_into().unwrap(),
        ))
        .unwrap();
        let reloc_size = usize::try_from(u32::from_le_bytes(
            raw_dll[opt_off + 140..opt_off + 144].try_into().unwrap(),
        ))
        .unwrap();

        if reloc_rva == 0 || reloc_size == 0 {
            return;
        }

        let mut cur = 0;
        while cur < reloc_size {
            let block_ptr = base_ptr.add(reloc_rva + cur);
            let block: ImageBaseRelocationBlock = ptr::read_unaligned(block_ptr.cast());
            let page_rva = usize::try_from(block.virtual_address.get()).unwrap();
            let block_size = usize::try_from(block.size_of_block.get()).unwrap();

            if block_size < 8 {
                break;
            }

            let num_entries = (block_size - 8) / 2;
            let entries_ptr = block_ptr.add(8).cast::<le::U16>();
            for i in 0..num_entries {
                let entry = ptr::read_unaligned(entries_ptr.add(i)).get();
                let rtype = entry >> 12;
                let roffset = usize::from(entry & 0x0FFF);

                if rtype == 3 {
                    // IMAGE_REL_BASED_HIGHLOW (32-bit delta adjustment)
                    let patch_addr = base_ptr.add(page_rva + roffset).cast::<le::U32>();
                    let old_val = ptr::read_unaligned(patch_addr).get();
                    let delta_u32 = u32::try_from(delta.cast_unsigned()).unwrap();
                    let new_val = old_val.wrapping_add(delta_u32);
                    ptr::write_unaligned(patch_addr, le::U32::new(new_val));
                }
            }
            cur += block_size;
        }
    }

    /// Resolves external Win32 / CRT functions into the Import Address Table (IAT)
    // SAFETY: Writing function pointers into pointer-aligned IAT slots.
    unsafe fn resolve_imports(base_ptr: *mut u8, raw_dll: &[u8]) {
        let pe_off =
            usize::try_from(u32::from_le_bytes(raw_dll[0x3c..0x40].try_into().unwrap())).unwrap();
        let opt_off = pe_off + 24;
        // DataDirectory[1] is Import Table (offset 104 in optional header)
        if opt_off + 112 > raw_dll.len() {
            return;
        }
        let imp_rva = usize::try_from(u32::from_le_bytes(
            raw_dll[opt_off + 104..opt_off + 108].try_into().unwrap(),
        ))
        .unwrap();
        let imp_size = usize::try_from(u32::from_le_bytes(
            raw_dll[opt_off + 108..opt_off + 112].try_into().unwrap(),
        ))
        .unwrap();

        if imp_rva == 0 || imp_size == 0 {
            return;
        }

        let mut desc_idx = 0;
        loop {
            let desc_ptr = base_ptr.add(imp_rva + desc_idx * 20).cast::<ImageImportDescriptor>();
            let desc: ImageImportDescriptor = ptr::read_unaligned(desc_ptr);
            let oft = usize::try_from(desc.original_first_thunk.get()).unwrap();
            let name_rva = usize::try_from(desc.name_rva.get()).unwrap();
            let ft = usize::try_from(desc.first_thunk.get()).unwrap();

            if name_rva == 0 && ft == 0 {
                break; // End of import descriptors
            }

            let dll_name_str = CStr::from_ptr(base_ptr.add(name_rva).cast::<std::ffi::c_char>())
                .to_string_lossy()
                .to_lowercase();

            let thunk_rva = if oft != 0 { oft } else { ft };
            let iat_rva = ft;

            let mut slot_idx = 0;
            loop {
                let thunk_entry =
                    ptr::read_unaligned(base_ptr.add(thunk_rva + slot_idx * 4).cast::<le::U32>())
                        .get();
                if thunk_entry == 0 {
                    break;
                }

                if (thunk_entry & 0x8000_0000) == 0 {
                    // Import by name
                    let name_entry_rva = usize::try_from(thunk_entry).unwrap();
                    let name_entry_ptr = base_ptr.add(name_entry_rva);
                    let func_name =
                        CStr::from_ptr(name_entry_ptr.add(2).cast::<std::ffi::c_char>())
                            .to_string_lossy();

                    let hook_ptr = resolve_import_symbol(&dll_name_str, &func_name);
                    eprintln!("[peldr import] {dll_name_str}!{func_name} -> {hook_ptr:?}");
                    ptr::write_unaligned(base_ptr.add(iat_rva + slot_idx * 4).cast(), hook_ptr);
                }

                slot_idx += 1;
            }

            desc_idx += 1;
        }
    }

    /// Resolves an exported function by name from the EAT (Export Address Table)
    pub fn get_export(&self, name: &str) -> Option<*mut c_void> {
        let base_ptr = self.base_addr.cast::<u8>();
        // SAFETY: Reading export directory and functions tables from mapped PE.
        unsafe {
            let pe_off =
                usize::try_from(ptr::read_unaligned(base_ptr.add(0x3c).cast::<u32>())).unwrap();
            let opt_off = pe_off + 24;
            let exp_rva = usize::try_from(ptr::read_unaligned(
                base_ptr.add(opt_off + 96).cast::<u32>(),
            ))
            .unwrap();
            if exp_rva == 0 {
                return None;
            }

            let exp: ImageExportDirectory = ptr::read_unaligned(base_ptr.add(exp_rva).cast());
            let num_functions = usize::try_from(exp.number_of_functions.get()).unwrap();
            let num_names = usize::try_from(exp.number_of_names.get()).unwrap();
            let funcs_rva = usize::try_from(exp.address_of_functions.get()).unwrap();
            let names_rva = usize::try_from(exp.address_of_names.get()).unwrap();
            let ords_rva = usize::try_from(exp.address_of_name_ordinals.get()).unwrap();

            (0..num_names).find_map(|i| {
                let n_rva = usize::try_from(
                    ptr::read_unaligned(base_ptr.add(names_rva + i * 4).cast::<le::U32>()).get(),
                )
                .unwrap();
                let sym_name = CStr::from_ptr(base_ptr.add(n_rva).cast::<std::ffi::c_char>())
                    .to_string_lossy();
                if sym_name == name {
                    let ord = usize::from(
                        ptr::read_unaligned(base_ptr.add(ords_rva + i * 2).cast::<le::U16>()).get(),
                    );
                    if ord < num_functions {
                        let fn_rva = usize::try_from(
                            ptr::read_unaligned(base_ptr.add(funcs_rva + ord * 4).cast::<le::U32>())
                                .get(),
                        )
                        .unwrap();
                        return Some(base_ptr.add(fn_rva).cast::<c_void>());
                    }
                }
                None
            })
        }
    }

    /// Resolves the `ZTypeIF` export
    #[allow(dead_code)]
    pub fn get_ztypeif(&self) -> Result<extern "C" fn(), &'static str> {
        let export_fn = self
            .get_export("ZTypeIF")
            .or_else(|| self.get_export("getinterface"))
            .ok_or("Failed to locate ZTypeIF export")?;
        // SAFETY: Transmuting verified raw export pointer to extern C function signature.
        Ok(unsafe { std::mem::transmute::<*mut c_void, extern "C" fn()>(export_fn) })
    }

    /// Resolves `ZTypeIF` or getinterface and initializes the DSP instance with host callback
    pub fn init_zfx_plugin(&self) -> Result<ZfxDspInstance, &'static str> {
        let export_fn = self
            .get_export("ZTypeIF")
            .or_else(|| self.get_export("getinterface"))
            .ok_or("Failed to locate ZTypeIF or getinterface export in PE")?;

        eprintln!("[peldr] Located getinterface export at {export_fn:?}");

        // Ensure Win32 TEB and FS segment register are initialized
        init_win32_teb();

        // SAFETY: Transmuting getinterface export pointer to function signature.
        let getinterface: extern "C" fn(host_callback: *const c_void) -> *mut c_void =
            unsafe { std::mem::transmute(export_fn) };

        // SAFETY: Converting default_audio_master host callback function pointer to raw C pointer.
        let host_cb_ptr = default_audio_master as *const c_void;
        eprintln!("[peldr] Calling getinterface({host_cb_ptr:?})...");
        // SAFETY: Invoking getinterface with default_audio_master host callback under Win32ContextGuard.
        let instance = unsafe {
            let _guard = Win32ContextGuard::enter();
            getinterface(host_cb_ptr)
        };
        eprintln!("[peldr] Returned from getinterface: instance={instance:?}");
        if instance.is_null() {
            return Err("getinterface returned null pointer");
        }

        let iface = instance.cast::<*mut c_void>();
        // SAFETY: Reading AEffect pointer from second slot of Zfx iface wrapper.
        let effect = unsafe { (*iface.add(1)).cast::<AEffect>() };
        if effect.is_null() {
            return Err("AEffect pointer is null");
        }
        // SAFETY: Reading magic number from non-null AEffect pointer.
        let magic = unsafe { (*effect).magic };
        if magic != 0x5673_7450 {
            return Err("AEffect magic does not match 'VstP'");
        }

        // SAFETY: Reading object pointer from valid AEffect instance.
        let obj = unsafe { (*effect).object.cast::<*mut usize>() };
        if !obj.is_null() {
            let obj_u8 = obj.cast::<u8>();
            // SAFETY: Checking if [obj + 0x328] is null and this is an HPS effect instance.
            unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let p_sub = obj_u8.add(0x328).cast::<*mut HpsPitchHelper>();
                if (*p_sub).is_null() {
                    let pe_bytes =
                        std::slice::from_raw_parts(self.base_addr.cast::<u8>(), self.size);
                    if pe_bytes
                        .windows(6)
                        .any(|w| w == [0x8b, 0x93, 0x28, 0x03, 0x00, 0x00])
                    {
                        eprintln!("[peldr] Installed HpsPitchHelper at [obj + 0x328]");
                        let helper = libc::malloc(std::mem::size_of::<HpsPitchHelper>()).cast::<HpsPitchHelper>();
                        if !helper.is_null() {
                            std::ptr::write(
                                helper,
                                HpsPitchHelper {
                                    vtable: HPS_VTABLE.0.as_ptr(),
                                    active: 1,
                                    pitch_index: 0,
                                },
                            );
                            *p_sub = helper;
                        }
                    }
                }
            }
        }

        let filter_adapter = self.scan_filter_adapter(effect);

        Ok(ZfxDspInstance {
            iface_wrapper: instance,
            effect,
            filter_adapter,
        })
    }

    /// Automatically scans mapped PE section memory and DSP object layout to construct the universal filter adapter.
    #[allow(
        clippy::too_many_lines,
        clippy::cast_possible_truncation,
        clippy::cognitive_complexity
    )]
    pub fn scan_filter_adapter(&self, effect: *mut AEffect) -> FilterAdapter {
        let mut adapter = FilterAdapter {
            biquad_offsets: [0; 8],
            num_biquads: 0,
            sr_f64_offset: None,
            sr_i32_offset: None,
            comb_len_offset: None,
            comb_buf_offset: None,
            comb_len_base: None,
            legacy_wah_ptr: None,
            legacy_wah_base: 0.0,
            legacy_pitch_ptrs: None,
            order5_offset: None,
            order5_base_a: [0.0; 5],
            order5_base_b: [0.0; 6],
            wah_offset: None,
            wah_base_multiplier: 0.0,
            comb_gain_base: None,
        };

        // SAFETY: Slicing the mapped PE image memory to locate sample rate branching opcodes.
        let mem = unsafe { std::slice::from_raw_parts(self.base_addr.cast::<u8>(), self.size) };
        let base = self.base_addr as usize;
        let end = base.saturating_add(self.size);

        // 1. Check for WahMultiplier pattern: cmp esi, 0xbb80 (\x81\xfe\x80\xbb\x00\x00)
        let wah_sig = [0x81u8, 0xfe, 0x80, 0xbb, 0x00, 0x00];
        if let Some(pos) = mem.windows(wah_sig.len()).position(|w| w == wah_sig) {
            let chunk = &mem[pos..pos.saturating_add(60).min(mem.len())];
            if let Some(je_rel) = chunk.windows(2).position(|w| w[0] == 0x74) {
                let fb_sub = &chunk[je_rel + 2..];
                if fb_sub.len() >= 8 && fb_sub[..4] == [0xf2, 0x0f, 0x10, 0x05] {
                    let fb_addr =
                        usize::try_from(u32::from_le_bytes(fb_sub[4..8].try_into().unwrap()))
                            .unwrap_or(0);
                    if let Some(jmp_rel) = fb_sub.windows(2).position(|w| w[0] == 0xeb) {
                        let k48_sub = &fb_sub[jmp_rel + 2..];
                        if k48_sub.len() >= 8 && k48_sub[..4] == [0xf2, 0x0f, 0x10, 0x05] {
                            let k48_addr = usize::try_from(u32::from_le_bytes(
                                k48_sub[4..8].try_into().unwrap(),
                            ))
                            .unwrap_or(0);
                            if fb_addr >= base
                                && fb_addr + 8 <= end
                                && k48_addr >= base
                                && k48_addr + 8 <= end
                            {
                                let fb_ptr = fb_addr as *mut f64;
                                let k48_ptr = k48_addr as *const f64;
                                // SAFETY: fb_ptr and k48_ptr verified within mapped PE module bounds.
                                let base_48k_val = unsafe { *k48_ptr };
                                eprintln!(
                                    "[peldr] Detected WahMultiplier: fallback={fb_ptr:?}, k48={k48_ptr:?}, base={base_48k_val}"
                                );
                                adapter.legacy_wah_ptr = Some(fb_ptr);
                                adapter.legacy_wah_base = base_48k_val;
                            }
                        }
                    }
                }
            }
        }

        // 2. Check for cmp eax, 0xbb80 (\x3d\x80\xbb\x00\x00) for PitchPoles
        let eax_sig = [0x3du8, 0x80, 0xbb, 0x00, 0x00];
        if let Some(pos) = mem.windows(eax_sig.len()).position(|w| w == eax_sig) {
            let chunk = &mem[pos..pos.saturating_add(300).min(mem.len())];
            let mut je_pos = None;
            let mut target_rel = 0usize;
            let mut hdr_len = 0usize;

            for (p, w) in chunk.windows(6).enumerate() {
                match w {
                    [0x74, rel, ..] => {
                        je_pos = Some(p);
                        target_rel = usize::from(*rel);
                        hdr_len = 2;
                        break;
                    }
                    [0x0f, 0x84, b0, b1, b2, b3] => {
                        je_pos = Some(p);
                        target_rel = usize::try_from(u32::from_le_bytes([*b0, *b1, *b2, *b3]))
                            .unwrap_or(0);
                        hdr_len = 6;
                        break;
                    }
                    _ => {}
                }
            }

            if let Some(jp) = je_pos {
                let fb_start = jp + hdr_len;
                let fb_end = (fb_start + target_rel).min(chunk.len());
                let fb_block = &chunk[fb_start..fb_end];

                let mut fb_ptrs = Vec::new();
                for i in 0..fb_block.len().saturating_sub(7) {
                    let w = &fb_block[i..i + 4];
                    if w == [0xf2, 0x0f, 0x10, 0x05]
                        || w == [0xf2, 0x0f, 0x10, 0x0d]
                        || w == [0xf2, 0x0f, 0x10, 0x15]
                    {
                        let addr = usize::try_from(u32::from_le_bytes(
                            fb_block[i + 4..i + 8].try_into().unwrap(),
                        ))
                        .unwrap_or(0);
                        if addr >= base && addr + 8 <= end {
                            fb_ptrs.push(addr as *mut f64);
                        }
                    }
                }

                let k48_start = fb_end;
                let k48_end = (k48_start + fb_block.len() + 40).min(chunk.len());
                let k48_block = &chunk[k48_start..k48_end];

                let mut k48_ptrs = Vec::new();
                for i in 0..k48_block.len().saturating_sub(7) {
                    let w = &k48_block[i..i + 4];
                    if w == [0xf2, 0x0f, 0x10, 0x05]
                        || w == [0xf2, 0x0f, 0x10, 0x0d]
                        || w == [0xf2, 0x0f, 0x10, 0x15]
                    {
                        let addr = usize::try_from(u32::from_le_bytes(
                            k48_block[i + 4..i + 8].try_into().unwrap(),
                        ))
                        .unwrap_or(0);
                        if addr >= base && addr + 8 <= end {
                            k48_ptrs.push(addr as *const f64);
                        }
                    }
                }

                if fb_ptrs.len() == 2 && k48_ptrs.len() >= 2 {
                    // SAFETY: k48_ptrs verified within mapped PE module bounds.
                    let k48_val0 = unsafe { *k48_ptrs[0] };
                    // SAFETY: k48_ptrs verified within mapped PE module bounds.
                    let k48_val1 = unsafe { *k48_ptrs[1] };
                    if ((0.70..0.85).contains(&k48_val0) && (0.90..0.99).contains(&k48_val1))
                        || ((0.70..0.85).contains(&k48_val1) && (0.90..0.99).contains(&k48_val0))
                    {
                        let (fb_tr, fb_sp, base_tr, base_sp) = if (0.70..0.85).contains(&k48_val0) {
                            (fb_ptrs[0], fb_ptrs[1], k48_val0, k48_val1)
                        } else {
                            (fb_ptrs[1], fb_ptrs[0], k48_val1, k48_val0)
                        };
                        eprintln!(
                            "[peldr] Detected PitchPoles: tracking={fb_tr:?} (base={base_tr}), splicing={fb_sp:?} (base={base_sp})"
                        );
                        adapter.legacy_pitch_ptrs = Some((fb_tr, fb_sp, base_tr, base_sp));
                    }
                }
            }
        }

        // 3. Object-level DSP structure inspection
        // SAFETY: Effect object pointer is initialized by the plugin entrypoint.
        let obj = unsafe { (*effect).object.cast::<*mut usize>() };
        if !obj.is_null() {
            // SAFETY: Reading vtable pointer from valid C++ object pointer.
            let vtable = unsafe { *obj };
            if !vtable.is_null() {
                // SAFETY: Index 6 in vtable is setSampleRate virtual method.
                let dsp_set_sr_fn = unsafe { *vtable.add(6) };
                if dsp_set_sr_fn != 0 {
                    // Initialize snapshots at 44.1k and 48k
                    CURRENT_SAMPLE_RATE.store(44100, Ordering::Relaxed);
                    // SAFETY: Invoking effOpen, effSetSampleRate, and setSampleRate virtual method at 44.1k under Win32ContextGuard.
                    unsafe {
                        let _guard = Win32ContextGuard::enter();
                        ((*effect).dispatcher)(effect, 0, 0, 0, std::ptr::null_mut(), 0.0);
                        ((*effect).dispatcher)(effect, 10, 0, 0, std::ptr::null_mut(), 44100.0);
                        std::arch::asm!(
                            "push {sr}",
                            "call {set_sr}",
                            sr = in(reg) 44100.0f32.to_bits(),
                            set_sr = in(reg) dsp_set_sr_fn,
                            in("ecx") obj,
                            clobber_abi("C"),
                        );
                    }
                    let mut snap_44k = [0u8; 0x300];
                    // SAFETY: Reading 0x300 bytes of DSP object memory for 44.1k snapshot.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            obj.cast::<u8>(),
                            snap_44k.as_mut_ptr(),
                            0x300,
                        );
                    }

                    CURRENT_SAMPLE_RATE.store(48000, Ordering::Relaxed);
                    // SAFETY: Invoking effSetSampleRate and setSampleRate virtual method at 48k under Win32ContextGuard.
                    unsafe {
                        let _guard = Win32ContextGuard::enter();
                        ((*effect).dispatcher)(effect, 10, 0, 0, std::ptr::null_mut(), 48000.0);
                        std::arch::asm!(
                            "push {sr}",
                            "call {set_sr}",
                            sr = in(reg) 48000.0f32.to_bits(),
                            set_sr = in(reg) dsp_set_sr_fn,
                            in("ecx") obj,
                            clobber_abi("C"),
                        );
                    }
                    let mut snap_48k = [0u8; 0x300];
                    // SAFETY: Reading 0x300 bytes of DSP object memory for 48k snapshot.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            obj.cast::<u8>(),
                            snap_48k.as_mut_ptr(),
                            0x300,
                        );
                    }

                    CURRENT_SAMPLE_RATE.store(96000, Ordering::Relaxed);
                    // SAFETY: Invoking effSetSampleRate and setSampleRate virtual method at 96k under Win32ContextGuard.
                    unsafe {
                        let _guard = Win32ContextGuard::enter();
                        ((*effect).dispatcher)(effect, 10, 0, 0, std::ptr::null_mut(), 96000.0);
                        std::arch::asm!(
                            "push {sr}",
                            "call {set_sr}",
                            sr = in(reg) 96000.0f32.to_bits(),
                            set_sr = in(reg) dsp_set_sr_fn,
                            in("ecx") obj,
                            clobber_abi("C"),
                        );
                    }
                    let mut snap_96k = [0u8; 0x300];
                    // SAFETY: Reading 0x300 bytes of DSP object memory for 96k snapshot.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            obj.cast::<u8>(),
                            snap_96k.as_mut_ptr(),
                            0x300,
                        );
                    }

                    // Restore back to 44.1k initial state
                    CURRENT_SAMPLE_RATE.store(44100, Ordering::Relaxed);
                    // SAFETY: Restoring 44.1k sample rate state via dispatcher and DSP vtable under Win32ContextGuard.
                    unsafe {
                        let _guard = Win32ContextGuard::enter();
                        ((*effect).dispatcher)(effect, 10, 0, 0, std::ptr::null_mut(), 44100.0);
                        std::arch::asm!(
                            "push {sr}",
                            "call {set_sr}",
                            sr = in(reg) 44100.0f32.to_bits(),
                            set_sr = in(reg) dsp_set_sr_fn,
                            in("ecx") obj,
                            clobber_abi("C"),
                        );
                    }

                    let view_44 = MemoryView::new(&snap_44k);
                    let view_48 = MemoryView::new(&snap_48k);
                    let view_96 = MemoryView::new(&snap_96k);

                    // Discover sample rate f64 offset
                    if let Some(off) = view_48.find_sr_f64(&view_44) {
                        eprintln!("[peldr] Discovered sr_f64_offset at {off:#x}");
                        adapter.sr_f64_offset = Some(off);
                    }

                    // Discover sample rate i32 offset
                    if let Some(off) = view_48.find_sr_i32(&view_44) {
                        eprintln!("[peldr] Discovered sr_i32_offset at {off:#x}");
                        adapter.sr_i32_offset = Some(off);
                    }

                    // Discover comb delay length offset and buffer pointer offset
                    if let Some(comb) = view_48.find_comb_filter(
                        &view_44,
                        is_plausible_heap_buf,
                        adapter.sr_f64_offset,
                        adapter.sr_i32_offset,
                    ) {
                        eprintln!(
                            "[peldr] Discovered comb_len_offset at {:#x} (48k={}, buf={:#x})",
                            comb.len_offset, comb.base_len, comb.buf_offset
                        );
                        adapter.comb_len_offset = Some(comb.len_offset);
                        adapter.comb_buf_offset = Some(comb.buf_offset);
                        adapter.comb_len_base = Some(comb.base_len);
                        adapter.comb_gain_base = comb.feedback_gain;
                    }

                    // Discover 5th-order cabinet / tone filter
                    if let Some((off, coeffs)) = view_48.find_order5(&view_44, &view_96) {
                        eprintln!(
                            "[peldr] Discovered 5th-order filter at offset {off:#x} (A1={}, A5={}, B0={})",
                            coeffs.a[0], coeffs.a[4], coeffs.b[0]
                        );
                        adapter.order5_offset = Some(off);
                        adapter.order5_base_a = coeffs.a;
                        adapter.order5_base_b = coeffs.b;
                    }

                    // Discover wah multiplier offset
                    if let Some((off, v48)) = view_48.find_wah_multiplier(&view_44) {
                        eprintln!(
                            "[peldr] Discovered wah multiplier at offset {off:#x} (base_48k={v48})"
                        );
                        adapter.wah_offset = Some(off);
                        adapter.wah_base_multiplier = v48;
                    }
                }
            }
        }

        adapter
    }
}

pub type LegacyPitchPoles = (*mut f64, *mut f64, f64, f64);

/// Universal sample-rate adaptation descriptor for a DSP plugin instance.
#[derive(Clone, Copy, Debug)]
pub struct FilterAdapter {
    pub biquad_offsets: [usize; 8],
    pub num_biquads: usize,
    pub sr_f64_offset: Option<usize>,
    pub sr_i32_offset: Option<usize>,
    pub comb_len_offset: Option<usize>,
    pub comb_buf_offset: Option<usize>,
    pub comb_len_base: Option<u32>,
    pub legacy_wah_ptr: Option<*mut f64>,
    pub legacy_wah_base: f64,
    pub legacy_pitch_ptrs: Option<LegacyPitchPoles>,
    pub order5_offset: Option<usize>,
    pub order5_base_a: [f64; 5],
    pub order5_base_b: [f64; 6],
    pub wah_offset: Option<usize>,
    pub wah_base_multiplier: f64,
    pub comb_gain_base: Option<f64>,
}

// SAFETY: FilterAdapter pointers refer to mapped PE section memory that remains valid for the module lifetime.
unsafe impl Send for FilterAdapter {}
// SAFETY: FilterAdapter does not mutate memory concurrently across threads.
unsafe impl Sync for FilterAdapter {}

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

#[derive(Clone, Copy)]
pub struct ZfxDspInstance {
    #[allow(dead_code)]
    pub iface_wrapper: *mut c_void,
    pub effect: *mut AEffect,
    pub filter_adapter: FilterAdapter,
}

// SAFETY: ZfxDspInstance wraps raw VST 2.4 pointers which are thread-safe when synchronized by the host.
unsafe impl Send for ZfxDspInstance {}
// SAFETY: Concurrent parameter inspection is guarded by the host audio processing loop.
unsafe impl Sync for ZfxDspInstance {}

impl ZfxDspInstance {
    pub fn open(&self) {
        // SAFETY: Invoking VST 2.4 effOpen dispatcher opcode under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 0 = effOpen in VST 2.4
            ((*self.effect).dispatcher)(self.effect, 0, 0, 0, ptr::null_mut(), 0.0);
        }
    }

    pub fn close(&self) {
        // SAFETY: Invoking VST 2.4 effClose dispatcher opcode under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 1 = effClose in VST 2.4
            ((*self.effect).dispatcher)(self.effect, 1, 0, 0, ptr::null_mut(), 0.0);
        }
    }

    pub fn num_params(&self) -> i32 {
        // SAFETY: Reading parameter count from validated effect pointer.
        unsafe { (*self.effect).num_params }
    }

    pub fn num_inputs(&self) -> i32 {
        // SAFETY: Reading input count from validated effect pointer.
        unsafe { (*self.effect).num_inputs }
    }

    pub fn num_outputs(&self) -> i32 {
        // SAFETY: Reading output count from validated effect pointer.
        unsafe { (*self.effect).num_outputs }
    }

    pub fn get_parameter_name(&self, index: i32) -> String {
        let mut buf = [0u8; 32];
        // SAFETY: Querying parameter name buffer from plugin dispatcher under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 8 = effGetParamName
            ((*self.effect).dispatcher)(
                self.effect,
                8,
                index,
                0,
                buf.as_mut_ptr().cast::<c_void>(),
                0.0,
            );
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..len]).trim().to_string()
    }

    pub fn set_parameter(&self, index: i32, value: f32) {
        // SAFETY: Invoking set_parameter function pointer on valid effect under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            ((*self.effect).set_parameter)(self.effect, index, value);
        }

        // Post-parameter hook: ensure sample rate fields remain set to current host rate
        let current_sr = CURRENT_SAMPLE_RATE.load(Ordering::Relaxed);
        if current_sr != 48000 && current_sr > 0 {
            // SAFETY: obj verified non-null.
            let obj_u8 = unsafe { (*self.effect).object.cast::<u8>() };
            if !obj_u8.is_null() {
                if let Some(off_f64) = self.filter_adapter.sr_f64_offset {
                    // SAFETY: off_f64 verified within DSP object bounds.
                    unsafe {
                        std::ptr::write_unaligned(
                            obj_u8.add(off_f64).cast::<f64>(),
                            f64::from(current_sr),
                        );
                    }
                }
                if let Some(off_i32) = self.filter_adapter.sr_i32_offset {
                    // SAFETY: off_i32 verified within DSP object bounds.
                    unsafe {
                        std::ptr::write_unaligned(obj_u8.add(off_i32).cast::<i32>(), current_sr);
                    }
                }
            }
        }
    }

    pub fn get_parameter(&self, index: i32) -> f32 {
        // SAFETY: Invoking get_parameter function pointer on valid effect under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            ((*self.effect).get_parameter)(self.effect, index)
        }
    }

    pub fn process_replacing_f64(
        &self,
        inputs: *const *const f64,
        outputs: *mut *mut f64,
        sample_frames: i32,
    ) {
        // SAFETY: Executing C++ virtual method for 64-bit audio replacement using x86 thiscall/fastcall ABI.
        unsafe {
            let obj = (*self.effect).object.cast::<*mut usize>();
            let vtable = *obj;
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
        // SAFETY: Reading channel counts from validated effect pointer.
        let dsp_ins = usize::try_from(unsafe { (*self.effect).num_inputs }.max(2)).unwrap_or(2);
        // SAFETY: Reading channel counts from validated effect pointer.
        let dsp_outs = usize::try_from(unsafe { (*self.effect).num_outputs }.max(2)).unwrap_or(2);

        // Prepare 64-bit input buffers
        let in_f64: Vec<Vec<f64>> = (0..dsp_ins)
            .map(|ch| {
                inputs
                    .get(ch)
                    .or_else(|| inputs.first())
                    .map_or_else(
                        || vec![0.0f64; frames],
                        |buf| buf.iter().map(|&s| f64::from(s)).collect(),
                    )
            })
            .collect();

        let mut out_f64: Vec<Vec<f64>> = (0..dsp_outs).map(|_| vec![0.0f64; frames]).collect();

        let in_ptrs: Vec<*const f64> = in_f64.iter().map(std::vec::Vec::as_ptr).collect();
        let mut out_ptrs: Vec<*mut f64> =
            out_f64.iter_mut().map(std::vec::Vec::as_mut_ptr).collect();

        let frames_i32 = i32::try_from(frames).unwrap_or(0);
        self.process_replacing_f64(in_ptrs.as_ptr(), out_ptrs.as_mut_ptr(), frames_i32);

        // Copy back to host outputs
        for (ch, out_buf) in outputs.iter_mut().enumerate() {
            let src_ch = if ch < dsp_outs { ch } else { 0 };
            if src_ch < out_f64.len() {
                for (s_out, &s_in) in out_buf.iter_mut().zip(&out_f64[src_ch]) {
                    #[allow(clippy::cast_possible_truncation)]
                    let s_f32 = s_in as f32;
                    *s_out = if s_in.is_finite() {
                        s_f32.clamp(-10.0, 10.0)
                    } else {
                        0.0
                    };
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn process_replacing(
        &self,
        inputs: *const *const f32,
        outputs: *mut *mut f32,
        sample_frames: i32,
    ) {
        // SAFETY: Invoking standard VST 2.4 process_replacing callback under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            ((*self.effect).process_replacing)(self.effect, inputs, outputs, sample_frames);
        }
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::too_many_lines,
        clippy::cognitive_complexity,
        clippy::cast_ptr_alignment
    )]
    pub fn set_sample_rate(&self, sample_rate: f32) {
        eprintln!("[peldr] set_sample_rate called with {sample_rate}");
        let sr_i32 = sample_rate as i32;
        CURRENT_SAMPLE_RATE.store(sr_i32, Ordering::Relaxed);

        // 1. Universal legacy migration: WahMultiplier & PitchPoles
        if let Some(fb_ptr) = self.filter_adapter.legacy_wah_ptr {
            let fs = f64::from(sample_rate);
            let k48 = self.filter_adapter.legacy_wah_base;
            let omega_a = 2.0 * 48000.0 * (k48 * 0.5).tan();
            let k_target = 2.0 * (omega_a / (2.0 * fs)).atan();
            // SAFETY: fb_ptr verified within mapped PE module bounds.
            unsafe {
                *fb_ptr = k_target;
            }
        }

        if let Some((fb_tr, fb_sp, base_tr, base_sp)) = self.filter_adapter.legacy_pitch_ptrs {
            let fs = f64::from(sample_rate);
            let target_tr = analog_proto::transform_single_pole(base_tr, 48000.0, fs);
            let target_sp = analog_proto::transform_single_pole(base_sp, 48000.0, fs);
            // SAFETY: pointers verified within mapped PE module bounds.
            unsafe {
                *fb_tr = target_tr;
                *fb_sp = target_sp;
            }
        }

        // 2. Sample rate core initialization:
        let base_sr = sample_rate;

        // SAFETY: Invoking VST 2.4 effSetSampleRate dispatcher opcode and DSP core vtable[6] under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 10 = effSetSampleRate in VST 2.4 (wrapper sets its internal sampleRate)
            ((*self.effect).dispatcher)(self.effect, 10, 0, 0, ptr::null_mut(), sample_rate);

            let obj = (*self.effect).object.cast::<*mut usize>();
            if !obj.is_null() {
                let vtable = *obj;
                if !vtable.is_null() {
                    let dsp_set_sr_fn = *vtable.add(6);
                    std::arch::asm!(
                        "push {sr}",
                        "call {set_sr}",
                        sr = in(reg) base_sr.to_bits(),
                        set_sr = in(reg) dsp_set_sr_fn,
                        in("ecx") obj,
                        clobber_abi("C"),
                    );
                }
            }
        }

        // 3. Overwrite internal sample rate fields with target sample rate
        // SAFETY: Reading effect object pointer.
        let obj_u8 = unsafe { (*self.effect).object.cast::<u8>() };
        if !obj_u8.is_null() {
            if let Some(off_f64) = self.filter_adapter.sr_f64_offset {
                // SAFETY: off_f64 verified within DSP object bounds.
                unsafe {
                    std::ptr::write_unaligned(
                        obj_u8.add(off_f64).cast::<f64>(),
                        f64::from(sample_rate),
                    );
                }
            }
            if let Some(off_i32) = self.filter_adapter.sr_i32_offset {
                // If this is HPS with a buffer at 0x148 sized to [obj + 0xec] * 8, resize buffer
                if off_i32 == 0xec {
                    // SAFETY: Checking buffer pointer at 0x148
                    unsafe {
                        let p_buf = obj_u8.add(0x148).cast::<*mut f64>();
                        let old_buf = std::ptr::read_unaligned(p_buf);
                        if is_plausible_heap_buf(old_buf as usize) {
                            let new_byte_size =
                                usize::try_from(sr_i32).unwrap_or(0) * std::mem::size_of::<f64>();
                            let new_buf =
                                libc::realloc(old_buf.cast::<libc::c_void>(), new_byte_size)
                                    .cast::<f64>();
                            if !new_buf.is_null() {
                                let old_u = old_buf as usize;
                                let new_u = new_buf as usize;
                                for slot in &TRACKED_ALLOCS {
                                    if slot
                                        .compare_exchange(
                                            old_u,
                                            new_u,
                                            Ordering::Relaxed,
                                            Ordering::Relaxed,
                                        )
                                        .is_ok()
                                    {
                                        break;
                                    }
                                }
                                std::ptr::write_unaligned(p_buf, new_buf);
                            }
                        }
                    }
                }
                // SAFETY: off_i32 verified within DSP object bounds.
                unsafe {
                    std::ptr::write_unaligned(obj_u8.add(off_i32).cast::<i32>(), sr_i32);
                }
            }

            // 4. Comb filter delay adaptation
            if let (Some(len_off), Some(base_len)) = (
                self.filter_adapter.comb_len_offset,
                self.filter_adapter.comb_len_base,
            ) {
                // SAFETY: comb offsets verified within DSP object bounds.
                unsafe {
                    let current_len = std::ptr::read_unaligned(obj_u8.add(len_off).cast::<u32>());
                    let ratio = f64::from(sample_rate) / 48000.0;
                    #[allow(clippy::cast_sign_loss)]
                    let new_len = (f64::from(base_len) * ratio).round() as u32;
                    if new_len != current_len && new_len > 0 {
                        if let Some(buf_off) = self.filter_adapter.comb_buf_offset {
                            let old_buf =
                                std::ptr::read_unaligned(obj_u8.add(buf_off).cast::<*mut f64>());
                            if is_plausible_heap_buf(old_buf as usize) {
                                let new_byte_size = usize::try_from(new_len).unwrap_or(0)
                                    * std::mem::size_of::<f64>();
                                let new_buf =
                                    libc::realloc(old_buf.cast::<libc::c_void>(), new_byte_size)
                                        .cast::<f64>();
                                if !new_buf.is_null() {
                                    let old_u = old_buf as usize;
                                    let new_u = new_buf as usize;
                                    for slot in &TRACKED_ALLOCS {
                                        if slot
                                            .compare_exchange(
                                                old_u,
                                                new_u,
                                                Ordering::Relaxed,
                                                Ordering::Relaxed,
                                            )
                                            .is_ok()
                                        {
                                            break;
                                        }
                                    }
                                    if new_len > current_len {
                                        let added_samples =
                                            usize::try_from(new_len - current_len).unwrap_or(0);
                                        let added_ptr =
                                            new_buf.add(usize::try_from(current_len).unwrap_or(0));
                                        std::ptr::write_bytes(added_ptr, 0, added_samples);
                                    }
                                    std::ptr::write_unaligned(
                                        obj_u8.add(buf_off).cast::<*mut f64>(),
                                        new_buf,
                                    );
                                    std::ptr::write_unaligned(
                                        obj_u8.add(len_off).cast::<u32>(),
                                        new_len,
                                    );
                                    eprintln!(
                                        "[peldr] Resized comb delay buffer: {current_len} -> {new_len} samples (base_48k={base_len})"
                                    );
                                }
                            }
                        } else {
                            std::ptr::write_unaligned(obj_u8.add(len_off).cast::<u32>(), new_len);
                            eprintln!(
                                "[peldr] Scaled inline comb/iter len: {current_len} -> {new_len} (base_48k={base_len})"
                            );
                        }
                    }
                }
            }

            // 5. Restore comb feedback gain if applicable
            if let (Some(len_off), Some(g_base)) = (
                self.filter_adapter.comb_len_offset,
                self.filter_adapter.comb_gain_base,
            ) {
                if len_off >= 8 {
                    // SAFETY: len_off verified within DSP object bounds.
                    unsafe {
                        std::ptr::write_unaligned(obj_u8.add(len_off - 8).cast::<f64>(), g_base);
                    }
                }
            }

            // 6. Wah multiplier adaptation
            if let Some(wah_off) = self.filter_adapter.wah_offset {
                let ratio = 48000.0 / f64::from(sample_rate);
                let target_wah = self.filter_adapter.wah_base_multiplier * ratio;
                // SAFETY: wah_off and sub-objects verified within DSP object bounds.
                unsafe {
                    std::ptr::write_unaligned(obj_u8.add(wah_off).cast::<f64>(), target_wah);

                    match wah_off {
                        0x1c8 => {
                            // Auto_Wah SVF range and min coefficients
                            std::ptr::write_unaligned(obj_u8.add(0x1a8).cast::<f64>(), 0.005 * ratio);
                            std::ptr::write_unaligned(obj_u8.add(0x1b0).cast::<f64>(), 0.03 * ratio);

                            let p_sub1 =
                                std::ptr::read_unaligned(obj_u8.add(0x134).cast::<*mut u8>());
                            if is_plausible_heap_buf(p_sub1 as usize) {
                                let mut params: WahResonatorParams =
                                    std::ptr::read_unaligned(p_sub1.cast());
                                params.param1 = 0.137_812_5 * ratio;
                                std::ptr::write_unaligned(p_sub1.cast(), params);
                            }
                            let p_sub3 =
                                std::ptr::read_unaligned(obj_u8.add(0x13c).cast::<*mut u8>());
                            if is_plausible_heap_buf(p_sub3 as usize) {
                                let mut params: WahResonatorParams =
                                    std::ptr::read_unaligned(p_sub3.cast());
                                params.param1 = 0.55 * ratio;
                                params.param2 = 0.1 * ratio;
                                std::ptr::write_unaligned(p_sub3.cast(), params);
                            }
                        }
                        0x1e0 => {
                            // PEDAL_CRY resonator normalized frequency parameter
                            let p_sub0 =
                                std::ptr::read_unaligned(obj_u8.add(0x114).cast::<*mut u8>());
                            if is_plausible_heap_buf(p_sub0 as usize) {
                                let mut params: WahResonatorParams =
                                    std::ptr::read_unaligned(p_sub0.cast());
                                params.param1 = 0.5 * ratio;
                                params.param2 = 0.05 * ratio;
                                std::ptr::write_unaligned(p_sub0.cast(), params);
                            }
                            std::ptr::write_unaligned(obj_u8.add(0x1a0).cast::<f64>(), 0.5 * ratio);
                            std::ptr::write_unaligned(obj_u8.add(0x1e8).cast::<f64>(), 0.5 * ratio);
                        }
                        _ => {}
                    }
                }
            }

            // 7. 5th-order cabinet / tone filter adaptation
            if let Some(off) = self.filter_adapter.order5_offset {
                let (b_new, a_new) = analog_proto::transform_order5(
                    self.filter_adapter.order5_base_b,
                    self.filter_adapter.order5_base_a,
                    48000.0,
                    f64::from(sample_rate),
                );
                let target_coeffs = Order5Coeffs { a: a_new, b: b_new };
                // SAFETY: off verified within DSP object bounds.
                unsafe {
                    std::ptr::write_unaligned(
                        obj_u8.add(off).cast::<Order5Coeffs>(),
                        target_coeffs,
                    );

                    // Also re-warp any channel sub-objects (e.g. at 0x1a8, 0x1ac, 0x1b0, etc.)
                    for sub_off in (0x100..0x200).step_by(4) {
                        let p_sub = std::ptr::read_unaligned(obj_u8.add(sub_off).cast::<*mut u8>());
                        if is_plausible_heap_buf(p_sub as usize) {
                            let sub_coeffs =
                                std::ptr::read_unaligned(p_sub.add(0x40).cast::<Order5Coeffs>());
                            if (4.0..5.5).contains(&sub_coeffs.a[0])
                                && (0.5..0.98).contains(&sub_coeffs.a[4])
                            {
                                std::ptr::write_unaligned(
                                    p_sub.add(0x40).cast::<Order5Coeffs>(),
                                    target_coeffs,
                                );
                            }
                        }
                    }
                }
            }

            // 8. Biquad re-warping if any discovered
            for &off in &self.filter_adapter.biquad_offsets[..self.filter_adapter.num_biquads] {
                // SAFETY: biquad offsets verified within DSP object bounds.
                unsafe {
                    let old_biquad =
                        std::ptr::read_unaligned(obj_u8.add(off).cast::<BiquadCoeffs>());
                    let (b_new, a_new) = analog_proto::transform_biquad(
                        old_biquad.b,
                        [1.0, old_biquad.a[0], old_biquad.a[1]],
                        48000.0,
                        f64::from(sample_rate),
                    );
                    let new_biquad = BiquadCoeffs {
                        b: b_new,
                        a: [a_new[1], a_new[2]],
                    };
                    std::ptr::write_unaligned(obj_u8.add(off).cast::<BiquadCoeffs>(), new_biquad);
                }
            }
        }
    }

    pub fn set_block_size(&self, block_size: i32) {
        CURRENT_BLOCK_SIZE.store(block_size, Ordering::Relaxed);
        // SAFETY: Invoking VST 2.4 effSetBlockSize dispatcher opcode and DSP core vtable[7] under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 11 = effSetBlockSize in VST 2.4
            ((*self.effect).dispatcher)(
                self.effect,
                11,
                0,
                isize::try_from(block_size).unwrap_or(0),
                ptr::null_mut(),
                0.0,
            );

            let obj = (*self.effect).object.cast::<*mut usize>();
            if !obj.is_null() {
                let vtable = *obj;
                if !vtable.is_null() {
                    let dsp_set_bs_fn = *vtable.add(7);
                    std::arch::asm!(
                        "push {bs}",
                        "call {set_bs}",
                        bs = in(reg) block_size,
                        set_bs = in(reg) dsp_set_bs_fn,
                        in("ecx") obj,
                        clobber_abi("C"),
                    );
                }
            }
        }
    }

    pub fn resume(&self) {
        // SAFETY: Invoking VST 2.4 effMainsChanged (start) dispatcher opcode under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 12 = effMainsChanged (1 = resume / start processing)
            ((*self.effect).dispatcher)(self.effect, 12, 0, 1, ptr::null_mut(), 0.0);
        }
    }

    pub fn suspend(&self) {
        // SAFETY: Invoking VST 2.4 effMainsChanged (stop) dispatcher opcode under Win32ContextGuard.
        unsafe {
            let _guard = Win32ContextGuard::enter();
            // Opcode 12 = effMainsChanged (0 = suspend / stop processing)
            ((*self.effect).dispatcher)(self.effect, 12, 0, 0, ptr::null_mut(), 0.0);
        }
    }
}

impl Drop for PeModule {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        // SAFETY: Unmapping memory allocated via libc::mmap during PeModule::load.
        unsafe {
            libc::munmap(self.base_addr, self.size);
        }
        #[cfg(target_os = "windows")]
        // SAFETY: Freeing memory allocated via VirtualAlloc during PeModule::load.
        unsafe {
            use windows_sys::Win32::System::Memory::{VirtualFree, MEM_RELEASE};
            VirtualFree(self.base_addr, 0, MEM_RELEASE);
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
    eprintln!("[default_audio_master] opcode={opcode}, index={index}, value={value}, opt={opt}");
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

// --- Import Symbol Dispatcher ---

// Dispatches imported DLL symbol names to function hook pointer addresses.
#[allow(
    clippy::too_many_lines,
    clippy::fn_to_numeric_cast
)]
fn resolve_import_symbol(dll: &str, func: &str) -> *mut c_void {
    match (dll, func) {
        // SETUPAPI.dll (Hardware Dongle Protection Spoofing)
        ("setupapi.dll", "SetupDiGetClassDevsA") => hook_SetupDiGetClassDevsA as *mut c_void,
        ("setupapi.dll", "SetupDiEnumDeviceInterfaces") => {
            hook_SetupDiEnumDeviceInterfaces as *mut c_void
        }
        ("setupapi.dll", "SetupDiGetDeviceInterfaceDetailA") => {
            hook_SetupDiGetDeviceInterfaceDetailA as *mut c_void
        }
        ("setupapi.dll", "SetupDiGetDeviceRegistryPropertyA") => {
            hook_SetupDiGetDeviceRegistryPropertyA as *mut c_void
        }
        ("setupapi.dll", "SetupDiDestroyDeviceInfoList") => {
            hook_SetupDiDestroyDeviceInfoList as *mut c_void
        }

        // KERNEL32.dll
        ("kernel32.dll", "InitializeCriticalSection") => {
            hook_InitializeCriticalSection as *mut c_void
        }
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
        ("kernel32.dll", "InterlockedCompareExchange") => {
            hook_InterlockedCompareExchange as *mut c_void
        }
        ("kernel32.dll", "InterlockedExchange") => hook_InterlockedExchange as *mut c_void,
        ("kernel32.dll", "IsDebuggerPresent") => hook_IsDebuggerPresent as *mut c_void,
        ("kernel32.dll", "Sleep") => hook_Sleep as *mut c_void,
        ("kernel32.dll", "SetUnhandledExceptionFilter") => {
            hook_SetUnhandledExceptionFilter as *mut c_void
        }
        ("kernel32.dll", "UnhandledExceptionFilter") => {
            hook_UnhandledExceptionFilter as *mut c_void
        }
        ("kernel32.dll", "TerminateProcess") => hook_TerminateProcess as *mut c_void,

        // MSVCR80.dll (Memory, String, and Math)
        ("msvcr80.dll", "_malloc_crt" | "??2@YAPAXI@Z" | "??_U@YAPAXI@Z") => {
            hook_malloc as *mut c_void
        }
        ("msvcr80.dll", "free" | "??3@YAXPAX@Z" | "??_V@YAXPAX@Z") => hook_free as *mut c_void,
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
        ("msvcr80.dll", "_initterm" | "_initterm_e") => hook_initterm as *mut c_void,
        ("msvcr80.dll", "_encode_pointer" | "_decode_pointer" | "__RTDynamicCast") => {
            hook_identity as *mut c_void
        }
        ("msvcr80.dll", "_encoded_null") => hook_null as *mut c_void,
        (
            "msvcr80.dll",
            "_lock"
            | "_unlock"
            | "_amsg_exit"
            | "_crt_debugger_hook"
            | "?terminate@@YAXXZ"
            | "?_type_info_dtor_internal_method@type_info@@QAEXXZ"
            | "__clean_type_info_names_internal",
        ) => hook_noop as *mut c_void,
        ("msvcr80.dll", "_onexit") => hook_onexit as *mut c_void,
        ("msvcr80.dll", "__dllonexit") => hook_dllonexit as *mut c_void,
        ("msvcr80.dll", "_purecall") => hook_purecall as *mut c_void,
        ("msvcr80.dll", "__CppXcptFilter" | "_except_handler4_common" | "__CxxFrameHandler3") => {
            hook_zero as *mut c_void
        }
        ("msvcr80.dll", "_adjust_fdiv") => (&raw mut ADJUST_FDIV).cast::<c_void>(),

        _ => {
            eprintln!("[peldr] Warning: Unhandled import {dll}!{func}");
            hook_noop as *mut c_void
        }
    }
}

// --- Win32 TEB and Thread Area Setup ---
#[cfg(target_os = "linux")]
#[repr(C)]
struct UserDesc {
    entry_number: u32,
    base_addr: u32,
    limit: u32,
    flags: u32,
}

#[cfg(target_os = "linux")]
static mut WIN32_TEB: [u32; 1024] = [0; 1024];
#[cfg(target_os = "linux")]
static mut TEB_SELECTOR: u16 = 0;

#[allow(clippy::missing_const_for_fn)]
pub fn init_win32_teb() {
    #[cfg(target_os = "linux")]
    // SAFETY: Setting up thread-local storage segment selector (FS) for Win32 TEB on x86 Linux.
    unsafe {
        if TEB_SELECTOR != 0 {
            return;
        }

        WIN32_TEB[0] = 0xFFFF_FFFF; // Head of SEH chain
        WIN32_TEB[6] = u32::try_from((&raw const WIN32_TEB).addr()).unwrap(); // Self pointer at TEB+0x18
        WIN32_TEB[8] = libc::getpid().cast_unsigned(); // Process ID at TEB+0x20
        WIN32_TEB[9] = libc::pthread_self(); // Thread ID at TEB+0x24

        let mut desc = UserDesc {
            entry_number: 0xFFFF_FFFF,
            base_addr: u32::try_from((&raw const WIN32_TEB).addr()).unwrap(),
            limit: 4096,
            flags: 1 | (1 << 6), // 32-bit segment + useable
        };

        // Syscall 243 is SYS_set_thread_area on i386
        let res = libc::syscall(243, &mut desc);
        if res == 0 {
            let sel = u16::try_from((desc.entry_number << 3) | 3).unwrap();
            TEB_SELECTOR = sel;
            eprintln!("[peldr] Win32 TEB initialized successfully! Selector={sel:#x}");
        } else {
            eprintln!("[peldr] Warning: SYS_set_thread_area returned {res}");
        }
    }
    #[cfg(target_os = "windows")]
    {
        // 32-bit Windows operating system automatically manages FS register and Win32 TEB.
    }
}

pub struct Win32ContextGuard {
    saved_fs: u16,
}

impl Win32ContextGuard {
    /// Saves the current FS segment register and loads the Win32 TEB selector.
    ///
    /// # Safety
    /// Must only be called on 32-bit x86 with a valid initialized Win32 TEB segment selector.
    #[inline]
    pub unsafe fn enter() -> Self {
        let mut saved_fs: u16 = 0;
        #[cfg(target_os = "linux")]
        let sel = TEB_SELECTOR;
        #[cfg(not(target_os = "linux"))]
        let sel = 0u16;
        if sel != 0 {
            // SAFETY: Swapping FS segment register with Win32 TEB selector.
            unsafe {
                std::arch::asm!(
                    "mov {saved:x}, fs",
                    "mov fs, {sel:x}",
                    saved = out(reg) saved_fs,
                    sel = in(reg) sel,
                    options(nostack, preserves_flags)
                );
            }
        }
        Self { saved_fs }
    }
}

impl Drop for Win32ContextGuard {
    #[inline]
    fn drop(&mut self) {
        if self.saved_fs != 0 {
            // SAFETY: Restoring original host Linux FS segment register.
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

static START_TIME: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

// --- SETUPAPI Trampolines (Hardware Dongle Spoofing) ---

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiGetClassDevsA(
    _class_guid: *const c_void,
    _enumerator: *const c_void,
    _hwnd_parent: *mut c_void,
    _flags: u32,
) -> *mut c_void {
    eprintln!("[hook_SetupDiGetClassDevsA]");
    ptr::without_provenance_mut(0x1337_0001)
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiEnumDeviceInterfaces(
    _device_info_set: *mut c_void,
    _device_info_data: *mut c_void,
    _interface_class_guid: *const c_void,
    member_index: u32,
    device_interface_data: *mut c_void,
) -> i32 {
    eprintln!("[hook_SetupDiEnumDeviceInterfaces] member_index={member_index}");
    if member_index == 0 {
        if !device_interface_data.is_null() {
            // SP_DEVICE_INTERFACE_DATA: cbSize is at offset 0, Flags is at offset 20
            let p = device_interface_data.cast::<u32>();
            // SAFETY: Writing active interface header data into caller-supplied structure.
            unsafe {
                *p = 28; // sizeof(SP_DEVICE_INTERFACE_DATA)
                *p.add(5) = 1; // SPINT_ACTIVE
            }
        }
        1 // TRUE: Zoom hardware device found!
    } else {
        0 // FALSE: No more items
    }
}

#[no_mangle]
#[allow(clippy::cast_ptr_alignment)]
pub unsafe extern "stdcall" fn hook_SetupDiGetDeviceInterfaceDetailA(
    _device_info_set: *mut c_void,
    _device_interface_data: *mut c_void,
    device_interface_detail_data: *mut c_void,
    device_interface_detail_data_size: u32,
    required_size: *mut u32,
    _device_info_data: *mut c_void,
) -> i32 {
    const SPOOFED_PATH: &[u8] =
        b"\\\\?\\usb#vid_1686&pid_0047#c51t#{a5dcbf10-11d2-6530-c000-1f90ed51b94f}\0";
    let needed = u32::try_from(4 + SPOOFED_PATH.len()).unwrap_or(0);

    if !required_size.is_null() {
        // SAFETY: Writing required size to caller buffer.
        unsafe {
            *required_size = needed;
        }
    }

    if !device_interface_detail_data.is_null() && device_interface_detail_data_size >= needed {
        // cbSize = 5 for SP_DEVICE_INTERFACE_DETAIL_DATA_A on 32-bit Windows
        let p = device_interface_detail_data.cast::<u8>();
        // SAFETY: Writing unaligned cbSize header and copying spoofed USB path.
        unsafe {
            ptr::write_unaligned(p.cast::<u32>(), 5);
            ptr::copy_nonoverlapping(SPOOFED_PATH.as_ptr(), p.add(4), SPOOFED_PATH.len());
        }
    }

    eprintln!(
        "[hook_SetupDiGetDeviceInterfaceDetailA] size={device_interface_detail_data_size}, needed={needed}"
    );
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
    eprintln!(
        "[hook_SetupDiGetDeviceRegistryPropertyA] prop={property}, buf_size={property_buffer_size}"
    );
    let id_len = u32::try_from(HARDWARE_ID.len()).unwrap_or(0);
    if !required_size.is_null() {
        // SAFETY: Writing required hardware ID length.
        unsafe {
            *required_size = id_len;
        }
    }
    if !property_reg_data_type.is_null() {
        // SAFETY: Setting registry type to REG_SZ (1).
        unsafe {
            *property_reg_data_type = 1;
        }
    }
    if !property_buffer.is_null() && property_buffer_size >= id_len {
        // SAFETY: Copying hardware ID string into caller buffer.
        unsafe {
            ptr::copy_nonoverlapping(HARDWARE_ID.as_ptr(), property_buffer, HARDWARE_ID.len());
        }
    }
    1
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_SetupDiDestroyDeviceInfoList(
    _device_info_set: *mut c_void,
) -> i32 {
    eprintln!("[hook_SetupDiDestroyDeviceInfoList]");
    1
}

// --- KERNEL32 Trampolines ---

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_InitializeCriticalSection(cs: *mut c_void) {
    if !cs.is_null() {
        // SAFETY: Zeroing 24-byte Win32 CRITICAL_SECTION structure.
        unsafe {
            ptr::write_bytes(cs.cast::<u8>(), 0, 24);
        }
    }
}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_DeleteCriticalSection(_cs: *mut c_void) {}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_EnterCriticalSection(_cs: *mut c_void) {}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_LeaveCriticalSection(_cs: *mut c_void) {}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_GetCurrentProcess() -> *mut c_void {
    ptr::without_provenance_mut(usize::MAX)
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetCurrentProcessId() -> u32 {
    #[cfg(unix)]
    {
        libc::getpid().cast_unsigned()
    }
    #[cfg(windows)]
    {
        windows_sys::Win32::System::Threading::GetCurrentProcessId()
    }
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetCurrentThreadId() -> u32 {
    #[cfg(unix)]
    {
        libc::pthread_self()
    }
    #[cfg(windows)]
    {
        windows_sys::Win32::System::Threading::GetCurrentThreadId()
    }
}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_GetLastError() -> u32 {
    0
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetTickCount() -> u32 {
    u32::try_from(START_TIME.elapsed().as_millis() & 0xFFFF_FFFF).unwrap_or(0)
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_GetSystemTimeAsFileTime(ft: *mut u64) {
    if !ft.is_null() {
        // SAFETY: Writing constant mock file time.
        unsafe {
            *ft = 130_000_000_000_000_000;
        }
    }
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_QueryPerformanceCounter(pc: *mut i64) -> i32 {
    if !pc.is_null() {
        // SAFETY: Writing high-resolution timestamp to caller pointer.
        unsafe {
            *pc =
                i64::try_from(START_TIME.elapsed().as_nanos() & 0x7FFF_FFFF_FFFF_FFFF).unwrap_or(0);
        }
    }
    1
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_InterlockedCompareExchange(
    dest: *mut i32,
    exchange: i32,
    comparand: i32,
) -> i32 {
    if dest.is_null() {
        return 0;
    }
    // SAFETY: Interlocked atomic operation on 32-bit aligned pointer.
    let atomic = unsafe { &*dest.cast::<AtomicI32>() };
    atomic
        .compare_exchange(comparand, exchange, Ordering::SeqCst, Ordering::SeqCst)
        .unwrap_or_else(|v| v)
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_InterlockedExchange(target: *mut i32, val: i32) -> i32 {
    if target.is_null() {
        return 0;
    }
    // SAFETY: Interlocked atomic swap on 32-bit aligned pointer.
    let atomic = unsafe { &*target.cast::<AtomicI32>() };
    atomic.swap(val, Ordering::SeqCst)
}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_IsDebuggerPresent() -> i32 {
    0
}

#[no_mangle]
pub unsafe extern "stdcall" fn hook_Sleep(ms: u32) {
    std::thread::sleep(std::time::Duration::from_millis(u64::from(ms)));
}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_SetUnhandledExceptionFilter(
    _f: *mut c_void,
) -> *mut c_void {
    ptr::null_mut()
}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_UnhandledExceptionFilter(_p: *mut c_void) -> i32 {
    0
}

#[no_mangle]
pub const unsafe extern "stdcall" fn hook_TerminateProcess(_h: *mut c_void, _code: u32) -> i32 {
    1
}

// --- MSVCR80 CRT Trampolines ---

#[no_mangle]
pub unsafe extern "C" fn hook_malloc(size: usize) -> *mut c_void {
    let p = libc::malloc(size);
    if !p.is_null() {
        let u = p as usize;
        for slot in &TRACKED_ALLOCS {
            if slot
                .compare_exchange(0, u, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }
    p
}

#[no_mangle]
pub unsafe extern "C" fn hook_free(ptr: *mut c_void) {
    if !ptr.is_null() {
        let u = ptr as usize;
        for slot in &TRACKED_ALLOCS {
            if slot
                .compare_exchange(u, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
        libc::free(ptr);
    }
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

extern "C" fn math_atan(x: f64) -> f64 {
    x.atan()
}
extern "C" fn math_cos(x: f64) -> f64 {
    x.cos()
}
extern "C" fn math_sin(x: f64) -> f64 {
    x.sin()
}
extern "C" fn math_exp(x: f64) -> f64 {
    x.exp()
}
extern "C" fn math_log(x: f64) -> f64 {
    x.ln()
}
extern "C" fn math_log10(x: f64) -> f64 {
    x.log10()
}
extern "C" fn math_pow(x: f64, y: f64) -> f64 {
    x.powf(y)
}
extern "C" fn math_tan(x: f64) -> f64 {
    x.tan()
}

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
pub unsafe extern "C" fn hook_initterm(
    start: *const Option<extern "C" fn()>,
    end: *const Option<extern "C" fn()>,
) -> i32 {
    let mut cur = start;
    while cur < end {
        // SAFETY: Reading function pointer from valid CRT init table.
        let f = unsafe { *cur };
        if let Some(func) = f {
            func();
        }
        // SAFETY: Incrementing pointer within table bounds.
        cur = unsafe { cur.add(1) };
    }
    0
}

#[no_mangle]
pub const unsafe extern "C" fn hook_identity(p: *mut c_void) -> *mut c_void {
    p
}

#[no_mangle]
pub const unsafe extern "C" fn hook_null() -> *mut c_void {
    ptr::null_mut()
}

#[no_mangle]
pub const unsafe extern "C" fn hook_noop() {}

#[no_mangle]
pub const unsafe extern "C" fn hook_zero() -> i32 {
    0
}

#[no_mangle]
pub const unsafe extern "C" fn hook_onexit(p: *mut c_void) -> *mut c_void {
    p
}

#[no_mangle]
pub const unsafe extern "C" fn hook_dllonexit(
    p: *mut c_void,
    _a: *mut c_void,
    _b: *mut c_void,
) -> *mut c_void {
    p
}

#[no_mangle]
pub unsafe extern "C" fn hook_purecall() {
    eprintln!("[peldr] purecall called!");
}

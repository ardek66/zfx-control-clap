//! DSP instance, filter adapter scan, and sample-rate adaptation.
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::Ordering;

use zerocopy::FromBytes;

use crate::analog_proto;
use crate::dsp_layout::{
    apply_order5_sr_adaptation, apply_wah_leaf_sr_side_effects, pitch_sr_owns_hps_buffer,
    BiquadCoeffs, CritSecKind, DEFAULT_OBJECT_PROBE_LEN, DSP_MODULE_BASE_END, DspFamily,
    FamilyScanWindow, MemoryView, Order5Coeffs, SrDiscoveryPolicy, HPS_HELPER_OBJECT_OFFSET,
    HPS_SR_BUFFER_PTR_OFFSET,
};
use super::hooks::{
    critsec_kind_from_snap_fallback, is_plausible_heap_buf, take_pending_critsec_kind,
    DspObjectConstructionGuard, KNOWN_DSP_OBJECT_SIZE, TRACKED_ALLOCS,
};
use super::pe_format::{X86MovsdDisp32, ZfxInterfaceWrapper};
use super::pe_image::PeModule;
use super::teb::{init_win32_teb, Win32ContextGuard};
use super::vst_host::{
    default_audio_master, AEffect, CURRENT_BLOCK_SIZE, CURRENT_SAMPLE_RATE,
};

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

impl PeModule {
    pub fn init_zfx_plugin(&self) -> Result<ZfxDspInstance, &'static str> {
        self.apply_zfx_image_patches();

        let export_fn = self
            .get_export("ZTypeIF")
            .or_else(|| self.get_export("getinterface"))
            .ok_or("Failed to locate ZTypeIF or getinterface export in PE")?;

        crate::worker_log!("[peldr] Located getinterface export at {export_fn:?}");

        // Ensure Win32 TEB and FS segment register are initialized
        init_win32_teb();

        // SAFETY: Transmuting getinterface export pointer to function signature.
        let getinterface: extern "C" fn(host_callback: *const c_void) -> *mut c_void =
            unsafe { std::mem::transmute(export_fn) };

        // SAFETY: Converting default_audio_master host callback function pointer to raw C pointer.
        let host_cb_ptr = default_audio_master as *const c_void;
        crate::worker_log!("[peldr] Calling getinterface({host_cb_ptr:?})...");
        // SAFETY: Invoking getinterface with default_audio_master host callback under Win32ContextGuard.
        let instance = unsafe {
            let _construct_guard = DspObjectConstructionGuard::enter();
            let _guard = Win32ContextGuard::enter();
            getinterface(host_cb_ptr)
        };
        crate::worker_log!("[peldr] Returned from getinterface: instance={instance:?}");
        if instance.is_null() {
            return Err("getinterface returned null pointer");
        }

        // SAFETY: Reading 8-byte ZfxInterfaceWrapper from verified non-null getinterface instance pointer.
        let wrapper: ZfxInterfaceWrapper = unsafe { ptr::read_unaligned(instance.cast()) };
        let effect = wrapper.effect_ptr.get() as *mut AEffect;
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
            // SAFETY: Checking if [obj + HPS_HELPER_OBJECT_OFFSET] is null for HPS-class plugins.
            // Offset confirmed via `mov edx,[ebx+0x328]` opcode scan; Pedal_Pitch (0x278) is too
            // small — only larger Harmonized Pitch Shifter objects use this slot.
            unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let p_sub = obj_u8
                    .add(HPS_HELPER_OBJECT_OFFSET)
                    .cast::<*mut HpsPitchHelper>();
                if (*p_sub).is_null() {
                    let pe_bytes =
                        std::slice::from_raw_parts(self.base_addr.cast::<u8>(), self.size);
                    if pe_bytes
                        .windows(6)
                        .any(|w| w == [0x8b, 0x93, 0x28, 0x03, 0x00, 0x00])
                    {
                        crate::worker_log!(
                            "[peldr] Installed HpsPitchHelper at [obj + {HPS_HELPER_OBJECT_OFFSET:#x}]"
                        );
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
        let known_object_size = KNOWN_DSP_OBJECT_SIZE.load(Ordering::Relaxed);
        let probe_len = known_object_size.max(DEFAULT_OBJECT_PROBE_LEN);
        let critsec_kind = take_pending_critsec_kind();

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
            dsp_family: DspFamily::Unknown,
            critsec_kind,
            object_probe_len: probe_len,
            scan_window: FamilyScanWindow::from_object(critsec_kind, probe_len),
        };

        // SAFETY: Slicing the mapped PE image memory to locate sample rate branching opcodes.
        let mem = unsafe { std::slice::from_raw_parts(self.base_addr.cast::<u8>(), self.size) };
        let base = self.base_addr as usize;
        let end = base.saturating_add(self.size);

        // Classify family from RTTI; constrained scans run later inside the family window.
        adapter.dsp_family = DspFamily::from_pe_rtti(mem);
        adapter.scan_window =
            FamilyScanWindow::from_object(adapter.critsec_kind, adapter.object_probe_len);
        crate::worker_log!(
            "[peldr] DspFamily::{:?} critsec={:?} probe_len={:#x} (scan window {:#x}..{:#x}, base_end={DSP_MODULE_BASE_END:#x})",
            adapter.dsp_family,
            adapter.critsec_kind,
            adapter.object_probe_len,
            adapter.scan_window.start,
            adapter.scan_window.end
        );

        // 1. Check for WahMultiplier pattern: cmp esi, 0xbb80 (\x81\xfe\x80\xbb\x00\x00)
        let wah_sig = [0x81u8, 0xfe, 0x80, 0xbb, 0x00, 0x00];
        if let Some(pos) = mem.windows(wah_sig.len()).position(|w| w == wah_sig) {
            let chunk = &mem[pos..pos.saturating_add(60).min(mem.len())];
            if let Some(je_rel) = chunk.windows(2).position(|w| w[0] == 0x74) {
                let fb_sub = &chunk[je_rel + 2..];
                if let Ok((fb_insn, _)) = X86MovsdDisp32::ref_from_prefix(fb_sub) {
                    if fb_insn.modrm == 0x05 {
                        let fb_addr = fb_insn.disp32.get() as usize;
                        if let Some(jmp_rel) = fb_sub.windows(2).position(|w| w[0] == 0xeb) {
                            let k48_sub = &fb_sub[jmp_rel + 2..];
                            if let Ok((k48_insn, _)) = X86MovsdDisp32::ref_from_prefix(k48_sub) {
                                if k48_insn.modrm == 0x05 {
                                    let k48_addr = k48_insn.disp32.get() as usize;
                                    if fb_addr >= base
                                        && fb_addr + 8 <= end
                                        && k48_addr >= base
                                        && k48_addr + 8 <= end
                                    {
                                        let fb_ptr = fb_addr as *mut f64;
                                        let k48_ptr = k48_addr as *const f64;
                                        // SAFETY: fb_ptr and k48_ptr verified within mapped PE module bounds.
                                        let base_48k_val = unsafe { *k48_ptr };
                                        crate::worker_log!(
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
                    if let Ok((insn, _)) = X86MovsdDisp32::ref_from_prefix(&fb_block[i..]) {
                        if insn.is_valid_disp32(base, end) {
                            fb_ptrs.push(insn.disp32.get() as *mut f64);
                        }
                    }
                }

                let k48_start = fb_end;
                let k48_end = (k48_start + fb_block.len() + 40).min(chunk.len());
                let k48_block = &chunk[k48_start..k48_end];

                let mut k48_ptrs = Vec::new();
                for i in 0..k48_block.len().saturating_sub(7) {
                    if let Ok((insn, _)) = X86MovsdDisp32::ref_from_prefix(&k48_block[i..]) {
                        if insn.is_valid_disp32(base, end) {
                            k48_ptrs.push(insn.disp32.get() as *const f64);
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
                        crate::worker_log!(
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
                    let probe_len = adapter.object_probe_len;
                    let mut snap_44k = vec![0u8; probe_len];
                    // SAFETY: Reading `probe_len` bytes of DSP object memory for 44.1k snapshot.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            obj.cast::<u8>(),
                            snap_44k.as_mut_ptr(),
                            probe_len,
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
                    let mut snap_48k = vec![0u8; probe_len];
                    // SAFETY: Reading `probe_len` bytes of DSP object memory for 48k snapshot.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            obj.cast::<u8>(),
                            snap_48k.as_mut_ptr(),
                            probe_len,
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
                    let mut snap_96k = vec![0u8; probe_len];
                    // SAFETY: Reading `probe_len` bytes of DSP object memory for 96k snapshot.
                    unsafe {
                        std::ptr::copy_nonoverlapping(
                            obj.cast::<u8>(),
                            snap_96k.as_mut_ptr(),
                            probe_len,
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

                    if adapter.critsec_kind == CritSecKind::Unknown {
                        adapter.critsec_kind = critsec_kind_from_snap_fallback(&snap_48k);
                        adapter.scan_window = FamilyScanWindow::from_object(
                            adapter.critsec_kind,
                            adapter.object_probe_len,
                        );
                        crate::worker_log!(
                            "[peldr] CritSec fallback from snap_48k: {:?}",
                            adapter.critsec_kind
                        );
                    }

                    let view_44 = MemoryView::new(&snap_44k);
                    let view_48 = MemoryView::new(&snap_48k);
                    let view_96 = MemoryView::new(&snap_96k);
                    let family = adapter.dsp_family;

                    // Evidence-backed SR discovery via SrDiscoveryPolicy (inverted Amp hint first).
                    let policy = if DspFamily::prefers_inverted_sr_from_rtti(mem) {
                        SrDiscoveryPolicy::InvertedPair
                    } else {
                        family.sr_discovery_policy()
                    };
                    let hit = view_48.discover_sr(
                        policy,
                        adapter.scan_window.as_range(),
                        &view_44,
                        &view_96,
                        family.also_try_packed_after_order5(),
                    );
                    // If inverted hint missed, fall back to the family's normal policy.
                    let hit = if !hit.found_sr()
                        && matches!(policy, SrDiscoveryPolicy::InvertedPair)
                    {
                        view_48.discover_sr(
                            family.sr_discovery_policy(),
                            adapter.scan_window.as_range(),
                            &view_44,
                            &view_96,
                            family.also_try_packed_after_order5(),
                        )
                    } else {
                        hit
                    };

                    if let Some(o5) = hit.order5_offset {
                        crate::worker_log!(
                            "[peldr] Order5→SR via {policy:?}: order5={o5:#x} sr_i32={:?}",
                            hit.sr_i32_offset
                        );
                        adapter.order5_offset = Some(o5);
                        if let Some(coeffs) = view_48.read_struct::<Order5Coeffs>(o5) {
                            adapter.order5_base_a = coeffs.a;
                            adapter.order5_base_b = coeffs.b;
                        }
                    }
                    if let Some(off) = hit.sr_i32_offset {
                        crate::worker_log!(
                            "[peldr] SR i32 at {off:#x} (policy={policy:?}, family={family:?})"
                        );
                        adapter.sr_i32_offset = Some(off);
                    }
                    if let Some(off) = hit.sr_f64_offset {
                        crate::worker_log!(
                            "[peldr] SR f64 at {off:#x} (policy={policy:?}, family={family:?})"
                        );
                        adapter.sr_f64_offset = Some(off);
                    }

                    // Discover comb delay length offset and buffer pointer offset
                    if let Some(comb) = view_48.find_comb_filter(
                        &view_44,
                        is_plausible_heap_buf,
                        adapter.sr_f64_offset,
                        adapter.sr_i32_offset,
                    ) {
                        crate::worker_log!(
                            "[peldr] Discovered comb_len_offset at {:#x} (48k={}, buf={:#x})",
                            comb.len_offset, comb.base_len, comb.buf_offset
                        );
                        adapter.comb_len_offset = Some(comb.len_offset);
                        adapter.comb_buf_offset = Some(comb.buf_offset);
                        adapter.comb_len_base = Some(comb.base_len);
                        adapter.comb_gain_base = comb.feedback_gain;
                    }

                    // Discover 5th-order cabinet / tone filter inside the family window.
                    if adapter.order5_offset.is_none() {
                        if let Some((off, coeffs)) =
                            view_48.find_order5_in(adapter.scan_window.as_range(), &view_44, &view_96)
                        {
                            crate::worker_log!(
                                "[peldr] Discovered 5th-order filter at offset {off:#x} (A1={}, A5={}, B0={})",
                                coeffs.a[0], coeffs.a[4], coeffs.b[0]
                            );
                            adapter.order5_offset = Some(off);
                            adapter.order5_base_a = coeffs.a;
                            adapter.order5_base_b = coeffs.b;
                        }
                    } else if let Some(off) = adapter.order5_offset {
                        if let Some(coeffs) = view_48.read_struct::<Order5Coeffs>(off) {
                            adapter.order5_base_a = coeffs.a;
                            adapter.order5_base_b = coeffs.b;
                        }
                    }

                    // Wah multiplier inside family window (tail scalars move per leaf).
                    if matches!(family, DspFamily::Wah) && adapter.wah_offset.is_none() {
                        if let Some((off, v48)) =
                            view_48.find_wah_mult_in(adapter.scan_window.as_range(), &view_44)
                        {
                            crate::worker_log!(
                                "[peldr] Discovered wah multiplier at offset {off:#x} (base_48k={v48})"
                            );
                            adapter.wah_offset = Some(off);
                            adapter.wah_base_multiplier = v48;
                        }
                    } else if adapter.wah_offset.is_none() {
                        if let Some((off, v48)) = view_48.find_wah_multiplier(&view_44) {
                            crate::worker_log!(
                                "[peldr] Discovered wah multiplier at offset {off:#x} (base_48k={v48})"
                            );
                            adapter.wah_offset = Some(off);
                            adapter.wah_base_multiplier = v48;
                        }
                    } else if let Some(off) = adapter.wah_offset {
                        if let Some(v48) = view_48.read_f64(off) {
                            adapter.wah_base_multiplier = v48;
                        }
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
    /// Family classified from PE RTTI / catalog (drives constrained scan rules).
    pub dsp_family: DspFamily,
    /// [`CritSecKind`] placement variant observed during DSP ctor.
    pub critsec_kind: CritSecKind,
    /// Bytes copied from the live DSP object for layout snapshots.
    pub object_probe_len: usize,
    /// Byte window for family-relative SR / Order5 / wah discovery.
    pub scan_window: FamilyScanWindow,
}

// SAFETY: FilterAdapter pointers refer to mapped PE section memory that remains valid for the module lifetime.
unsafe impl Send for FilterAdapter {}
// SAFETY: FilterAdapter does not mutate memory concurrently across threads.
unsafe impl Sync for FilterAdapter {}

#[derive(Clone, Copy)]
pub struct ZfxDspInstance {
    #[allow(dead_code)]
    pub iface_wrapper: *mut c_void,
    pub effect: *mut AEffect,
    pub filter_adapter: FilterAdapter,
}

/// f64 planes for the guest DSP, allocated once when the plugin is loaded.
///
/// Zoom's engines dereference at least two input pointers and two output
/// pointers, including on mono plugs.
pub struct AudioScratch {
    inputs: Box<[Box<[f64]>]>,
    outputs: Box<[Box<[f64]>]>,
    in_ptrs: Box<[*const f64]>,
    out_ptrs: Box<[*mut f64]>,
}

impl AudioScratch {
    #[must_use]
    pub fn new(input_channels: usize, output_channels: usize) -> Self {
        let n_in = input_channels.max(2);
        let n_out = output_channels.max(2);
        let inputs = (0..n_in)
            .map(|_| vec![0.0f64; crate::protocol::MAX_FRAMES].into_boxed_slice())
            .collect::<Box<[_]>>();
        let outputs = (0..n_out)
            .map(|_| vec![0.0f64; crate::protocol::MAX_FRAMES].into_boxed_slice())
            .collect::<Box<[_]>>();
        let in_ptrs = vec![std::ptr::null(); n_in].into_boxed_slice();
        let out_ptrs = vec![std::ptr::null_mut(); n_out].into_boxed_slice();
        Self {
            inputs,
            outputs,
            in_ptrs,
            out_ptrs,
        }
    }

    pub fn render(
        &mut self,
        inst: &ZfxDspInstance,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        let frames = inputs
            .first()
            .map_or_else(|| outputs.first().map_or(0, |ch| ch.len()), |ch| ch.len())
            .min(crate::protocol::MAX_FRAMES);
        if frames == 0 {
            return;
        }

        for (channel, plane) in self.inputs.iter_mut().enumerate() {
            let source = inputs.get(channel).copied().or_else(|| inputs.first().copied());
            match source {
                Some(samples) => {
                    for (dst, &src) in plane[..frames].iter_mut().zip(samples) {
                        *dst = f64::from(src);
                    }
                    let copied = frames.min(samples.len());
                    plane[copied..frames].fill(0.0);
                }
                None => plane[..frames].fill(0.0),
            }
        }
        for plane in &mut self.outputs {
            plane[..frames].fill(0.0);
        }
        for (ptr, plane) in self.in_ptrs.iter_mut().zip(self.inputs.iter()) {
            *ptr = plane.as_ptr();
        }
        for (ptr, plane) in self.out_ptrs.iter_mut().zip(self.outputs.iter_mut()) {
            *ptr = plane.as_mut_ptr();
        }

        let frames_i32 = i32::try_from(frames).unwrap_or(0);
        inst.process_replacing_f64(self.in_ptrs.as_ptr(), self.out_ptrs.as_mut_ptr(), frames_i32);

        for (ch, out_buf) in outputs.iter_mut().enumerate() {
            let src_ch = if ch < self.outputs.len() { ch } else { 0 };
            let Some(plane) = self.outputs.get(src_ch) else {
                continue;
            };
            let count = out_buf.len().min(frames);
            for (dst, &src) in out_buf[..count].iter_mut().zip(&plane[..count]) {
                #[allow(clippy::cast_possible_truncation)]
                let sample = src as f32;
                *dst = if src.is_finite() {
                    sample.clamp(-10.0, 10.0)
                } else {
                    0.0
                };
            }
        }
    }
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
        crate::worker_log!("[peldr] set_sample_rate called with {sample_rate}");
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
                // HPS-only: buffer at 0x148 is sized to sr_i32@0xec * 8.
                // Many PackedSr plugins also use 0xec/0xf0 (Analog_Delay, Auto_Wah, …) —
                // reallocating 0x148 there corrupts delay state and can hang/crash the DAW.
                if pitch_sr_owns_hps_buffer(self.filter_adapter.dsp_family, off_i32) {
                    // SAFETY: Checking HPS buffer pointer on Pitch objects only.
                    unsafe {
                        let p_buf = obj_u8.add(HPS_SR_BUFFER_PTR_OFFSET).cast::<*mut f64>();
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
                                    crate::worker_log!(
                                        "[peldr] Resized comb delay buffer: {current_len} -> {new_len} samples (base_48k={base_len})"
                                    );
                                }
                            }
                        } else {
                            std::ptr::write_unaligned(obj_u8.add(len_off).cast::<u32>(), new_len);
                            crate::worker_log!(
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
                // SAFETY: wah_off and leaf clusters verified within DSP object bounds.
                unsafe {
                    std::ptr::write_unaligned(obj_u8.add(wah_off).cast::<f64>(), target_wah);
                    apply_wah_leaf_sr_side_effects(obj_u8, wah_off, ratio, is_plausible_heap_buf);
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
                // SAFETY: off and Order5 sub-object window verified within DSP object bounds.
                unsafe {
                    apply_order5_sr_adaptation(obj_u8, off, target_coeffs, is_plausible_heap_buf);
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

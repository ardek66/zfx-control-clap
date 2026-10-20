//! Win32 / CRT / math import hooks and DSP construction tracking.
use std::cell::Cell;
use std::ffi::c_void;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::time::Instant;

use zerocopy::little_endian as le;

use crate::dsp_layout::{CritSecKind, CRITSEC_SIZE, DSP_MODULE_BASE_END};
use super::pe_format::{SpDeviceInterfaceDetailDataHeader, Win32CriticalSection};

const MAX_TRACKED_ALLOCS: usize = 1024;
pub static TRACKED_ALLOCS: [AtomicUsize; MAX_TRACKED_ALLOCS] =
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

/// Largest `operator_new` / malloc size seen while constructing the DSP via getinterface.
pub static KNOWN_DSP_OBJECT_SIZE: AtomicUsize = AtomicUsize::new(0);
static DSP_OBJECT_UNDER_CONSTRUCTION: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// `(object_base, object_size)` while the Zoom DSP ctor runs inside getinterface.
    static CURRENT_DSP_OBJECT: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
    static PENDING_CRITSEC_KIND: Cell<CritSecKind> = const { Cell::new(CritSecKind::Unknown) };
}

/// Active during getinterface so malloc / `CritSec` hooks can attribute layout to the DSP object.
pub struct DspObjectConstructionGuard;

impl DspObjectConstructionGuard {
    pub fn enter() -> Self {
        DSP_OBJECT_UNDER_CONSTRUCTION.store(true, Ordering::Relaxed);
        KNOWN_DSP_OBJECT_SIZE.store(0, Ordering::Relaxed);
        CURRENT_DSP_OBJECT.with(|c| c.set((0, 0)));
        PENDING_CRITSEC_KIND.with(|c| c.set(CritSecKind::Unknown));
        Self
    }
}

impl Drop for DspObjectConstructionGuard {
    fn drop(&mut self) {
        DSP_OBJECT_UNDER_CONSTRUCTION.store(false, Ordering::Relaxed);
        CURRENT_DSP_OBJECT.with(|c| c.set((0, 0)));
    }
}

#[must_use]
const fn critsec_kind_from_offset(off: usize) -> CritSecKind {
    match off {
        0xd0 => CritSecKind::At0D0,
        0xd4 => CritSecKind::At0D4,
        0xe8 => CritSecKind::At0E8,
        0x100 => CritSecKind::At0100,
        _ => CritSecKind::Unknown,
    }
}

pub fn take_pending_critsec_kind() -> CritSecKind {
    PENDING_CRITSEC_KIND.with(Cell::get)
}

fn record_critsec_kind_from_hook(cs: *mut c_void) {
    if cs.is_null() {
        return;
    }
    let cs_addr = cs.cast::<u8>() as usize;
    CURRENT_DSP_OBJECT.with(|cell| {
        let (base, size) = cell.get();
        if base == 0 || size == 0 {
            return;
        }
        let end = base.saturating_add(size);
        if cs_addr < base || cs_addr.saturating_add(CRITSEC_SIZE) > end {
            return;
        }
        let off = cs_addr - base;
        let kind = critsec_kind_from_offset(off);
        if kind != CritSecKind::Unknown {
            PENDING_CRITSEC_KIND.with(|pending| pending.set(kind));
        }
    });
}

fn snap_region_looks_like_initialized_critsec(region: &[u8]) -> bool {
    region.len() >= CRITSEC_SIZE && region[..CRITSEC_SIZE].iter().all(|&b| b == 0)
}

pub fn critsec_kind_from_snap_fallback(snap: &[u8]) -> CritSecKind {
    for off in [0xd0_usize, 0xd4, 0xe8, 0x100] {
        if off + CRITSEC_SIZE <= snap.len() {
            let region = &snap[off..off + CRITSEC_SIZE];
            if snap_region_looks_like_initialized_critsec(region) {
                return critsec_kind_from_offset(off);
            }
        }
    }
    CritSecKind::Unknown
}


#[allow(clippy::too_many_lines, clippy::fn_to_numeric_cast)]
pub fn resolve_import_symbol(dll: &str, func: &str) -> *mut c_void {
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
            crate::worker_log!("[peldr] Warning: Unhandled import {dll}!{func}");
            #[cfg(debug_assertions)]
            {
                // Fail-loud in debug so newly observed Zoom imports are not silently no-op'd.
                panic!("[peldr] unhandled import {dll}!{func}");
            }
            #[cfg(not(debug_assertions))]
            {
                hook_noop as *mut c_void
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
    crate::worker_log!("[hook_SetupDiGetClassDevsA]");
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
    crate::worker_log!("[hook_SetupDiEnumDeviceInterfaces] member_index={member_index}");
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
    let hdr_len = std::mem::size_of::<SpDeviceInterfaceDetailDataHeader>();
    let needed = u32::try_from(hdr_len + SPOOFED_PATH.len()).unwrap_or(0);

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
            ptr::write_unaligned(
                p.cast::<SpDeviceInterfaceDetailDataHeader>(),
                SpDeviceInterfaceDetailDataHeader {
                    cb_size: le::U32::new(5),
                },
            );
            ptr::copy_nonoverlapping(SPOOFED_PATH.as_ptr(), p.add(hdr_len), SPOOFED_PATH.len());
        }
    }

    crate::worker_log!(
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
    crate::worker_log!(
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
    crate::worker_log!("[hook_SetupDiDestroyDeviceInfoList]");
    1
}

// --- KERNEL32 Trampolines ---

#[no_mangle]
pub unsafe extern "stdcall" fn hook_InitializeCriticalSection(cs: *mut c_void) {
    record_critsec_kind_from_hook(cs);
    if !cs.is_null() {
        // SAFETY: Initializing 24-byte Win32 CRITICAL_SECTION structure.
        unsafe {
            ptr::write_unaligned(
                cs.cast::<Win32CriticalSection>(),
                Win32CriticalSection {
                    debug_info: le::U32::ZERO,
                    lock_count: le::I32::ZERO,
                    recursion_count: le::I32::ZERO,
                    owning_thread: le::U32::ZERO,
                    lock_semaphore: le::U32::ZERO,
                    spin_count: le::U32::ZERO,
                },
            );
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
        if DSP_OBJECT_UNDER_CONSTRUCTION.load(Ordering::Relaxed) && size >= DSP_MODULE_BASE_END {
            KNOWN_DSP_OBJECT_SIZE.fetch_max(size, Ordering::Relaxed);
            CURRENT_DSP_OBJECT.with(|cell| {
                let (_, prev_size) = cell.get();
                if size >= prev_size {
                    cell.set((u, size));
                }
            });
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
    crate::worker_log!("[peldr] purecall called!");
}

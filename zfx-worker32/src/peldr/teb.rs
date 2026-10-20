//! Win32 TEB / FS-segment setup for guest DSP threads on Linux.
use zerocopy::little_endian as le;

use super::pe_format::Win32Tib;

#[cfg(target_os = "linux")]
#[repr(C)]
struct UserDesc {
    entry_number: u32,
    base_addr: u32,
    limit: u32,
    flags: u32,
}

#[cfg(target_os = "linux")]
static mut WIN32_TIB: Win32Tib = Win32Tib {
    current_seh: le::U32::ZERO,
    stack_base: le::U32::ZERO,
    stack_limit: le::U32::ZERO,
    sub_system_tib: le::U32::ZERO,
    fiber_data: le::U32::ZERO,
    arbitrary_user_data: le::U32::ZERO,
    self_tib: le::U32::ZERO,
    env_pointer: le::U32::ZERO,
    process_id: le::U32::ZERO,
    thread_id: le::U32::ZERO,
    rpc_handle: le::U32::ZERO,
    tls_array: le::U32::ZERO,
    peb_ptr: le::U32::ZERO,
    last_error: le::U32::ZERO,
    padding: [0; 4096 - 0x38],
};
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

        let tib_addr = u32::try_from((&raw const WIN32_TIB).addr()).unwrap();
        WIN32_TIB.current_seh = le::U32::new(0xFFFF_FFFF);
        WIN32_TIB.self_tib = le::U32::new(tib_addr);
        WIN32_TIB.process_id = le::U32::new(libc::getpid().cast_unsigned());
        WIN32_TIB.thread_id = le::U32::new(libc::pthread_self());

        let mut desc = UserDesc {
            entry_number: 0xFFFF_FFFF,
            base_addr: tib_addr,
            limit: 4096,
            flags: 1 | (1 << 6), // 32-bit segment + useable
        };

        // Syscall 243 is SYS_set_thread_area on i386
        let res = libc::syscall(243, &mut desc);
        if res == 0 {
            let sel = u16::try_from((desc.entry_number << 3) | 3).unwrap();
            TEB_SELECTOR = sel;
            crate::worker_log!("[peldr] Win32 TEB initialized successfully! Selector={sel:#x}");
        } else {
            crate::worker_log!("[peldr] Warning: SYS_set_thread_area returned {res}");
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

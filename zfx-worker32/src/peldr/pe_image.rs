//! PE image mapping: mmap/VirtualAlloc, section copy, Drop/munmap.
use goblin::pe::PE;
use std::ffi::c_void;
use std::ptr;

use std::ffi::CStr;

use zerocopy::little_endian as le;

use super::pe_format::{
    ImageDosHeader, ImageExportDirectory, ImageNtHeaders32, IMAGE_DIRECTORY_ENTRY_EXPORT,
};
use super::pe_iat::resolve_imports;
use super::pe_reloc::apply_relocations;

/// Represents our embedded, memory-mapped Windows DLL.
pub struct PeModule {
    pub base_addr: *mut c_void,
    pub size: usize,
}

impl PeModule {
    /// Loads a PE image from raw bytes into executable memory.
    ///
    /// Stops at mapped + relocated + imported. Zoom-specific patches are applied
    /// later by [`Self::apply_zfx_image_patches`] from [`Self::init_zfx_plugin`].
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
        crate::worker_log!(
            "[peldr] base_ptr={base_ptr:?}, image_base={image_base:#x}, delta={delta:#x}"
        );
        if delta != 0 {
            // SAFETY: Applying relocations across valid mapped PE section memory.
            unsafe {
                apply_relocations(base_ptr, raw_dll_bytes, delta, size_of_image)?;
            }
        }
        crate::worker_log!("[peldr] Relocations applied successfully");

        // 5. Resolve Imports into the IAT
        // SAFETY: Resolving imported Win32 and CRT functions into the mapped IAT.
        unsafe {
            resolve_imports(base_ptr, raw_dll_bytes, size_of_image)?;
        }
        crate::worker_log!("[peldr] Imports resolved successfully");

        Ok(Self {
            base_addr,
            size: size_of_image,
        })
    }

    /// Resolves an exported function by name from the EAT (Export Address Table).
    pub fn get_export(&self, name: &str) -> Option<*mut c_void> {
        let base_ptr = self.base_addr.cast::<u8>();
        let size_of_image = self.size;
        // SAFETY: Reading export directory and functions tables from mapped PE.
        unsafe {
            if size_of_image < std::mem::size_of::<ImageDosHeader>() {
                return None;
            }
            let dos_hdr: ImageDosHeader = ptr::read_unaligned(base_ptr.cast());
            let pe_off = usize::try_from(dos_hdr.e_lfanew.get()).ok()?;
            if pe_off.checked_add(std::mem::size_of::<ImageNtHeaders32>())? > size_of_image {
                return None;
            }
            let nt_hdr: ImageNtHeaders32 = ptr::read_unaligned(base_ptr.add(pe_off).cast());
            let exp_dir = &nt_hdr.optional_header.data_directories[IMAGE_DIRECTORY_ENTRY_EXPORT];
            let exp_rva = usize::try_from(exp_dir.virtual_address.get()).ok()?;
            if exp_rva == 0 {
                return None;
            }
            if exp_rva.checked_add(std::mem::size_of::<ImageExportDirectory>())? > size_of_image {
                return None;
            }

            let exp: ImageExportDirectory = ptr::read_unaligned(base_ptr.add(exp_rva).cast());
            let num_functions = usize::try_from(exp.number_of_functions.get()).ok()?;
            let num_names = usize::try_from(exp.number_of_names.get()).ok()?;
            let funcs_rva = usize::try_from(exp.address_of_functions.get()).ok()?;
            let names_rva = usize::try_from(exp.address_of_names.get()).ok()?;
            let ords_rva = usize::try_from(exp.address_of_name_ordinals.get()).ok()?;

            (0..num_names).find_map(|i| {
                let name_ptr_off = names_rva.checked_add(i.checked_mul(4)?)?;
                if name_ptr_off.checked_add(4)? > size_of_image {
                    return None;
                }
                let n_rva = usize::try_from(
                    ptr::read_unaligned(base_ptr.add(name_ptr_off).cast::<le::U32>()).get(),
                )
                .ok()?;
                if n_rva >= size_of_image {
                    return None;
                }
                let sym_name = CStr::from_ptr(base_ptr.add(n_rva).cast::<std::ffi::c_char>())
                    .to_string_lossy();
                if sym_name == name {
                    let ord_off = ords_rva.checked_add(i.checked_mul(2)?)?;
                    if ord_off.checked_add(2)? > size_of_image {
                        return None;
                    }
                    let ord = usize::from(
                        ptr::read_unaligned(base_ptr.add(ord_off).cast::<le::U16>()).get(),
                    );
                    if ord < num_functions {
                        let fn_off = funcs_rva.checked_add(ord.checked_mul(4)?)?;
                        if fn_off.checked_add(4)? > size_of_image {
                            return None;
                        }
                        let fn_rva = usize::try_from(
                            ptr::read_unaligned(base_ptr.add(fn_off).cast::<le::U32>()).get(),
                        )
                        .ok()?;
                        if fn_rva >= size_of_image {
                            return None;
                        }
                        return Some(base_ptr.add(fn_rva).cast::<c_void>());
                    }
                }
                None
            })
        }
    }

    /// Resolves the `ZTypeIF` export.
    #[allow(dead_code)]
    pub fn get_ztypeif(&self) -> Result<extern "C" fn(), &'static str> {
        let export_fn = self
            .get_export("ZTypeIF")
            .or_else(|| self.get_export("getinterface"))
            .ok_or("Failed to locate ZTypeIF export")?;
        // SAFETY: Transmuting verified raw export pointer to extern C function signature.
        Ok(unsafe { std::mem::transmute::<*mut c_void, extern "C" fn()>(export_fn) })
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

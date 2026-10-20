//! Import Address Table resolution and Export Address Table lookup.
use std::ffi::CStr;
use std::ptr;

use zerocopy::{little_endian as le, FromBytes};

use super::hooks::resolve_import_symbol;
use super::pe_format::{
    ImageDosHeader, ImageImportByNameHeader, ImageImportDescriptor, ImageNtHeaders32,
    IMAGE_DIRECTORY_ENTRY_IMPORT,
};

/// Resolves external Win32 / CRT functions into the Import Address Table (IAT).
///
/// # Safety
/// `base_ptr` must point to a writable mapped PE image of `size_of_image` bytes whose
/// import directory and IAT slots are valid for the parsed headers in `raw_dll`.
#[allow(clippy::too_many_lines)]
pub unsafe fn resolve_imports(
    base_ptr: *mut u8,
    raw_dll: &[u8],
    size_of_image: usize,
) -> Result<(), &'static str> {
    let (dos_hdr, _) =
        ImageDosHeader::ref_from_prefix(raw_dll).map_err(|_| "iat: bad DOS header")?;
    let pe_off =
        usize::try_from(dos_hdr.e_lfanew.get()).map_err(|_| "iat: e_lfanew overflow")?;
    let nt_slice = raw_dll.get(pe_off..).ok_or("iat: NT header OOB")?;
    let (nt_hdr, _) =
        ImageNtHeaders32::ref_from_prefix(nt_slice).map_err(|_| "iat: bad NT header")?;
    let imp_dir = &nt_hdr.optional_header.data_directories[IMAGE_DIRECTORY_ENTRY_IMPORT];
    let imp_rva = usize::try_from(imp_dir.virtual_address.get())
        .map_err(|_| "iat: import RVA overflow")?;
    let imp_size =
        usize::try_from(imp_dir.size.get()).map_err(|_| "iat: import size overflow")?;

    if imp_rva == 0 || imp_size == 0 {
        return Ok(());
    }
    if imp_rva
        .checked_add(imp_size)
        .is_none_or(|end| end > size_of_image)
    {
        return Err("iat: import directory exceeds SizeOfImage");
    }

    let mut desc_idx = 0usize;
    loop {
        let desc_off = imp_rva
            .checked_add(desc_idx.checked_mul(20).ok_or("iat: desc index overflow")?)
            .ok_or("iat: descriptor offset overflow")?;
        if desc_off
            .checked_add(20)
            .is_none_or(|end| end > size_of_image)
        {
            return Err("iat: import descriptor OOB");
        }
        // SAFETY: descriptor offset verified within SizeOfImage.
        let desc_ptr = unsafe { base_ptr.add(desc_off).cast::<ImageImportDescriptor>() };
        // SAFETY: Reading import descriptor from mapped PE.
        let desc: ImageImportDescriptor = unsafe { ptr::read_unaligned(desc_ptr) };
        let oft =
            usize::try_from(desc.original_first_thunk.get()).map_err(|_| "iat: OFT overflow")?;
        let name_rva =
            usize::try_from(desc.name_rva.get()).map_err(|_| "iat: name RVA overflow")?;
        let ft = usize::try_from(desc.first_thunk.get()).map_err(|_| "iat: FT overflow")?;

        if name_rva == 0 && ft == 0 {
            break;
        }
        if name_rva >= size_of_image || ft >= size_of_image {
            return Err("iat: DLL name or IAT RVA OOB");
        }

        // SAFETY: name_rva checked against SizeOfImage; CStr reads until NUL within image ideally.
        let dll_name_str =
            unsafe { CStr::from_ptr(base_ptr.add(name_rva).cast::<std::ffi::c_char>()) }
                .to_string_lossy()
                .to_lowercase();

        let thunk_rva = if oft != 0 { oft } else { ft };
        let iat_rva = ft;
        if thunk_rva >= size_of_image {
            return Err("iat: thunk RVA OOB");
        }

        let mut slot_idx = 0usize;
        loop {
            let thunk_off = thunk_rva
                .checked_add(slot_idx.checked_mul(4).ok_or("iat: thunk slot overflow")?)
                .ok_or("iat: thunk offset overflow")?;
            if thunk_off
                .checked_add(4)
                .is_none_or(|end| end > size_of_image)
            {
                return Err("iat: thunk entry OOB");
            }
            let iat_off = iat_rva
                .checked_add(slot_idx * 4)
                .ok_or("iat: IAT slot overflow")?;
            if iat_off
                .checked_add(4)
                .is_none_or(|end| end > size_of_image)
            {
                return Err("iat: IAT write OOB");
            }

            // SAFETY: thunk_off verified within SizeOfImage.
            let thunk_entry =
                unsafe { ptr::read_unaligned(base_ptr.add(thunk_off).cast::<le::U32>()) }.get();
            if thunk_entry == 0 {
                break;
            }

            if (thunk_entry & 0x8000_0000) == 0 {
                let name_entry_rva = usize::try_from(thunk_entry)
                    .map_err(|_| "iat: import-by-name RVA overflow")?;
                let name_hdr_end = name_entry_rva
                    .checked_add(std::mem::size_of::<ImageImportByNameHeader>())
                    .ok_or("iat: import-by-name header overflow")?;
                if name_hdr_end > size_of_image {
                    return Err("iat: import-by-name OOB");
                }
                // SAFETY: name entry within SizeOfImage; function name is NUL-terminated in PE.
                let name_ptr = unsafe {
                    base_ptr
                        .add(name_entry_rva)
                        .add(std::mem::size_of::<ImageImportByNameHeader>())
                };
                // SAFETY: name_ptr points at a NUL-terminated import name within the mapped image.
                let func_name = unsafe { CStr::from_ptr(name_ptr.cast::<std::ffi::c_char>()) }
                    .to_string_lossy();

                let hook_ptr = resolve_import_symbol(&dll_name_str, &func_name);
                crate::worker_log!("[peldr import] {dll_name_str}!{func_name} -> {hook_ptr:?}");
                // SAFETY: IAT slot offset verified within SizeOfImage.
                unsafe {
                    ptr::write_unaligned(base_ptr.add(iat_off).cast(), hook_ptr);
                }
            }

            slot_idx += 1;
        }

        desc_idx += 1;
    }
    Ok(())
}

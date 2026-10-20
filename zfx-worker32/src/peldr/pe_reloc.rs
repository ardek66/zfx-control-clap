//! PE base relocation application.
use std::ptr;

use zerocopy::{little_endian as le, FromBytes};

use super::pe_format::{
    ImageBaseRelocationBlock, ImageDosHeader, ImageNtHeaders32, IMAGE_DIRECTORY_ENTRY_BASERELOC,
};

/// Applies `IMAGE_REL_BASED_HIGHLOW` relocations.
///
/// # Safety
/// `base_ptr` must point to a writable mapped PE image of at least `size_of_image` bytes.
/// `raw_dll` must contain valid PE headers for that image.
pub unsafe fn apply_relocations(
    base_ptr: *mut u8,
    raw_dll: &[u8],
    delta: isize,
    size_of_image: usize,
) -> Result<(), &'static str> {
    let (dos_hdr, _) =
        ImageDosHeader::ref_from_prefix(raw_dll).map_err(|_| "reloc: bad DOS header")?;
    let pe_off =
        usize::try_from(dos_hdr.e_lfanew.get()).map_err(|_| "reloc: e_lfanew overflow")?;
    let nt_slice = raw_dll.get(pe_off..).ok_or("reloc: NT header OOB")?;
    let (nt_hdr, _) =
        ImageNtHeaders32::ref_from_prefix(nt_slice).map_err(|_| "reloc: bad NT header")?;
    let reloc_dir = &nt_hdr.optional_header.data_directories[IMAGE_DIRECTORY_ENTRY_BASERELOC];
    let reloc_rva = usize::try_from(reloc_dir.virtual_address.get())
        .map_err(|_| "reloc: reloc RVA overflow")?;
    let reloc_size =
        usize::try_from(reloc_dir.size.get()).map_err(|_| "reloc: reloc size overflow")?;

    if reloc_rva == 0 || reloc_size == 0 {
        return Ok(());
    }
    if reloc_rva.checked_add(reloc_size).is_none_or(|end| end > size_of_image) {
        return Err("reloc: directory exceeds SizeOfImage");
    }

    let mut cur = 0usize;
    while cur < reloc_size {
        if reloc_rva + cur + 8 > size_of_image {
            return Err("reloc: block header OOB");
        }
        // SAFETY: block_ptr is within SizeOfImage as checked above.
        let block_ptr = unsafe { base_ptr.add(reloc_rva + cur) };
        // SAFETY: Reading an 8-byte reloc block header from mapped image memory.
        let block: ImageBaseRelocationBlock =
            unsafe { ptr::read_unaligned(block_ptr.cast()) };
        let page_rva = usize::try_from(block.virtual_address.get())
            .map_err(|_| "reloc: page RVA overflow")?;
        let block_size =
            usize::try_from(block.size_of_block.get()).map_err(|_| "reloc: block size overflow")?;

        if block_size < 8 {
            break;
        }
        if cur.checked_add(block_size).is_none_or(|end| end > reloc_size) {
            return Err("reloc: block exceeds reloc directory");
        }
        if reloc_rva + cur + block_size > size_of_image {
            return Err("reloc: block exceeds SizeOfImage");
        }

        let num_entries = (block_size - 8) / 2;
        // SAFETY: entries lie within the verified reloc block.
        let entries_ptr = unsafe { block_ptr.add(8).cast::<le::U16>() };
        for i in 0..num_entries {
            // SAFETY: `i` is within num_entries of the verified block.
            let entry = unsafe { ptr::read_unaligned(entries_ptr.add(i)) }.get();
            let rtype = entry >> 12;
            let roffset = usize::from(entry & 0x0FFF);

            if rtype == 3 {
                // IMAGE_REL_BASED_HIGHLOW
                let patch_off = page_rva
                    .checked_add(roffset)
                    .ok_or("reloc: patch offset overflow")?;
                if patch_off
                    .checked_add(4)
                    .is_none_or(|end| end > size_of_image)
                {
                    return Err("reloc: patch target exceeds SizeOfImage");
                }
                // SAFETY: patch_addr is bounds-checked against SizeOfImage.
                let patch_addr = unsafe { base_ptr.add(patch_off).cast::<le::U32>() };
                // SAFETY: patch_addr bounds-checked against SizeOfImage above.
                let old_val = unsafe { ptr::read_unaligned(patch_addr) }.get();
                let delta_u32 = u32::try_from(delta.cast_unsigned())
                    .map_err(|_| "reloc: delta does not fit u32")?;
                let new_val = old_val.wrapping_add(delta_u32);
                // SAFETY: Writing relocated u32 back to the verified patch address.
                unsafe {
                    ptr::write_unaligned(patch_addr, le::U32::new(new_val));
                }
            }
        }
        cur += block_size;
    }
    Ok(())
}

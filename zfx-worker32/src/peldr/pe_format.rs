//! PE / Win32 POD layouts and x86 patch opcodes for the in-memory loader.
use zerocopy::{little_endian as le, FromBytes, Immutable, IntoBytes, KnownLayout};

/// Win32 DOS Executable Stub Header (64 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
#[allow(clippy::struct_field_names)]
pub struct ImageDosHeader {
    pub e_magic: [u8; 2], // Magic number "MZ" (0x5A4D)
    pub e_cblp: le::U16,
    pub e_cp: le::U16,
    pub e_crlc: le::U16,
    pub e_cparhdr: le::U16,
    pub e_minalloc: le::U16,
    pub e_maxalloc: le::U16,
    pub e_ss: le::U16,
    pub e_sp: le::U16,
    pub e_csum: le::U16,
    pub e_ip: le::U16,
    pub e_cs: le::U16,
    pub e_lfarlc: le::U16,
    pub e_ovno: le::U16,
    pub e_res: [le::U16; 4],
    pub e_oemid: le::U16,
    pub e_oeminfo: le::U16,
    pub e_res2: [le::U16; 10],
    pub e_lfanew: le::U32, // File address of new exe header (0x3C)
}

/// Win32 Data Directory Entry (8 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, Default)]
pub struct ImageDataDirectory {
    pub virtual_address: le::U32,
    pub size: le::U32,
}

pub const IMAGE_DIRECTORY_ENTRY_EXPORT: usize = 0;
pub const IMAGE_DIRECTORY_ENTRY_IMPORT: usize = 1;
#[allow(dead_code)]
pub const IMAGE_DIRECTORY_ENTRY_RESOURCE: usize = 2;
#[allow(dead_code)]
pub const IMAGE_DIRECTORY_ENTRY_EXCEPTION: usize = 3;
#[allow(dead_code)]
pub const IMAGE_DIRECTORY_ENTRY_SECURITY: usize = 4;
pub const IMAGE_DIRECTORY_ENTRY_BASERELOC: usize = 5;
#[allow(dead_code)]
pub const IMAGE_DIRECTORY_ENTRY_DEBUG: usize = 6;
#[allow(dead_code)]
pub const IMAGE_DIRECTORY_ENTRY_TLS: usize = 9;
#[allow(dead_code)]
pub const IMAGE_DIRECTORY_ENTRY_IAT: usize = 12;

/// Standard Win32 COFF File Header (20 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageFileHeader {
    pub machine: le::U16,
    pub number_of_sections: le::U16,
    pub time_date_stamp: le::U32,
    pub pointer_to_symbol_table: le::U32,
    pub number_of_symbols: le::U32,
    pub size_of_optional_header: le::U16,
    pub characteristics: le::U16,
}

/// Standard Win32 32-bit Optional Header (224 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageOptionalHeader32 {
    pub magic: le::U16, // 0x10B = PE32
    pub major_linker_version: u8,
    pub minor_linker_version: u8,
    pub size_of_code: le::U32,
    pub size_of_initialized_data: le::U32,
    pub size_of_uninitialized_data: le::U32,
    pub address_of_entry_point: le::U32,
    pub base_of_code: le::U32,
    pub base_of_data: le::U32,
    pub image_base: le::U32,
    pub section_alignment: le::U32,
    pub file_alignment: le::U32,
    pub major_operating_system_version: le::U16,
    pub minor_operating_system_version: le::U16,
    pub major_image_version: le::U16,
    pub minor_image_version: le::U16,
    pub major_subsystem_version: le::U16,
    pub minor_subsystem_version: le::U16,
    pub win32_version_value: le::U32,
    pub size_of_image: le::U32,
    pub size_of_headers: le::U32,
    pub check_sum: le::U32,
    pub subsystem: le::U16,
    pub dll_characteristics: le::U16,
    pub size_of_stack_reserve: le::U32,
    pub size_of_stack_commit: le::U32,
    pub size_of_heap_reserve: le::U32,
    pub size_of_heap_commit: le::U32,
    pub loader_flags: le::U32,
    pub number_of_rva_and_sizes: le::U32,
    pub data_directories: [ImageDataDirectory; 16],
}

/// Win32 NT Headers for 32-bit PE (248 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageNtHeaders32 {
    pub signature: [u8; 4], // "PE\0\0" (0x00004550)
    pub file_header: ImageFileHeader,
    pub optional_header: ImageOptionalHeader32,
}

/// Win32 Section Table Header (40 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageSectionHeader {
    pub name: [u8; 8],
    pub virtual_size: le::U32,
    pub virtual_address: le::U32,
    pub size_of_raw_data: le::U32,
    pub pointer_to_raw_data: le::U32,
    pub pointer_to_relocations: le::U32,
    pub pointer_to_linenumbers: le::U32,
    pub number_of_relocations: le::U16,
    pub number_of_linenumbers: le::U16,
    pub characteristics: le::U32,
}

/// Win32 Import By Name Header (2 bytes hint followed by ASCII name).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ImageImportByNameHeader {
    pub hint: le::U16,
}

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

/// Standard Win32 Thread Information Block (TIB) on x86 (first 0x38 bytes of TEB).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct Win32Tib {
    pub current_seh: le::U32,         // 0x00: FS:[0x00] Head of SEH chain
    pub stack_base: le::U32,          // 0x04: FS:[0x04]
    pub stack_limit: le::U32,         // 0x08: FS:[0x08]
    pub sub_system_tib: le::U32,      // 0x0C: FS:[0x0C]
    pub fiber_data: le::U32,          // 0x10: FS:[0x10]
    pub arbitrary_user_data: le::U32, // 0x14: FS:[0x14]
    pub self_tib: le::U32,            // 0x18: FS:[0x18] Linear address of TIB
    pub env_pointer: le::U32,         // 0x1C: FS:[0x1C]
    pub process_id: le::U32,          // 0x20: FS:[0x20] Win32 PID
    pub thread_id: le::U32,           // 0x24: FS:[0x24] Win32 TID
    pub rpc_handle: le::U32,          // 0x28: FS:[0x28]
    pub tls_array: le::U32,           // 0x2C: FS:[0x2C]
    pub peb_ptr: le::U32,             // 0x30: FS:[0x30] Process Environment Block
    pub last_error: le::U32,          // 0x34: FS:[0x34]
    pub padding: [u8; 4096 - 0x38],   // Pad to full 4KB page
}

/// Win32 Critical Section object (24 bytes).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, Default)]
pub struct Win32CriticalSection {
    pub debug_info: le::U32,
    pub lock_count: le::I32,
    pub recursion_count: le::I32,
    pub owning_thread: le::U32,
    pub lock_semaphore: le::U32,
    pub spin_count: le::U32,
}

/// Win32 `SP_DEVICE_INTERFACE_DETAIL_DATA_A` Header (4 bytes cbSize).
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct SpDeviceInterfaceDetailDataHeader {
    pub cb_size: le::U32,
}

/// ZFX Plugin Wrapper returned by `getinterface` export.
#[repr(C)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy)]
pub struct ZfxInterfaceWrapper {
    pub vtable_or_host: le::U32, // Slot 0: Pointer to vtable / host callback
    pub effect_ptr: le::U32,     // Slot 1: Pointer to VST 2.4 AEffect instance
}

/// x86 `sub esp, imm32` instruction encoding (6 bytes).
#[repr(C, packed)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
pub struct X86SubEspImm32 {
    pub opcode: [u8; 2], // [0x81, 0xEC]
    pub imm32: le::U32,  // 0x0000044C
}

/// Win32 x86 Hot-Patch: `mov al, 1; ret; nop; nop; nop` (6 bytes).
#[repr(C, packed)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DongleBypassPatch {
    pub mov_al_1: [u8; 2], // [0xB0, 0x01]
    pub ret: u8,          // 0xC3
    pub nops: [u8; 3],    // [0x90, 0x90, 0x90]
}

/// x86 `jmp rel32; nop` hot-patch (6 bytes).
#[repr(C, packed)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
pub struct X86JmpRel32Nop {
    pub opcode: u8,     // 0xE9
    pub rel32: le::I32, // Relative displacement
    pub nop: u8,        // 0x90
}

/// x86 SSE `movsd xmm*, qword ptr [disp32]` instruction (8 bytes).
/// Matches `0xf2, 0x0f, 0x10, modrm, disp32`.
#[repr(C, packed)]
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Debug, Clone, Copy, PartialEq, Eq)]
pub struct X86MovsdDisp32 {
    pub prefix: u8,      // 0xF2
    pub opcode: [u8; 2], // [0x0F, 0x10]
    pub modrm: u8,       // 0x05 (xmm0), 0x0D (xmm1), 0x15 (xmm2)
    pub disp32: le::U32, // 32-bit absolute address in PE data section
}

impl X86MovsdDisp32 {
    #[must_use]
    pub fn is_valid_disp32(&self, base: usize, end: usize) -> bool {
        let addr = self.disp32.get() as usize;
        self.prefix == 0xF2
            && self.opcode == [0x0F, 0x10]
            && (self.modrm == 0x05 || self.modrm == 0x0D || self.modrm == 0x15)
            && addr >= base
            && addr.saturating_add(8) <= end
    }
}

const _: () = assert!(std::mem::size_of::<ImageDosHeader>() == 64);
const _: () = assert!(std::mem::size_of::<ImageDataDirectory>() == 8);
const _: () = assert!(std::mem::size_of::<ImageFileHeader>() == 20);
const _: () = assert!(std::mem::size_of::<ImageOptionalHeader32>() == 224);
const _: () = assert!(std::mem::size_of::<ImageNtHeaders32>() == 248);
const _: () = assert!(std::mem::size_of::<ImageSectionHeader>() == 40);
const _: () = assert!(std::mem::size_of::<ImageImportByNameHeader>() == 2);
const _: () = assert!(std::mem::size_of::<ImageBaseRelocationBlock>() == 8);
const _: () = assert!(std::mem::size_of::<ImageImportDescriptor>() == 20);
const _: () = assert!(std::mem::size_of::<ImageExportDirectory>() == 40);
const _: () = assert!(std::mem::size_of::<Win32Tib>() == 4096);
const _: () = assert!(std::mem::size_of::<Win32CriticalSection>() == 24);
const _: () = assert!(std::mem::size_of::<SpDeviceInterfaceDetailDataHeader>() == 4);
const _: () = assert!(std::mem::size_of::<ZfxInterfaceWrapper>() == 8);
const _: () = assert!(std::mem::size_of::<X86SubEspImm32>() == 6);
const _: () = assert!(std::mem::size_of::<DongleBypassPatch>() == 6);
const _: () = assert!(std::mem::size_of::<X86JmpRel32Nop>() == 6);
const _: () = assert!(std::mem::size_of::<X86MovsdDisp32>() == 8);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pe_pod_sizes_match_win32_layout() {
        assert_eq!(std::mem::size_of::<ImageDosHeader>(), 64);
        assert_eq!(std::mem::size_of::<ImageNtHeaders32>(), 248);
        assert_eq!(std::mem::size_of::<ImageSectionHeader>(), 40);
        assert_eq!(std::mem::size_of::<ImageImportDescriptor>(), 20);
        assert_eq!(std::mem::size_of::<ImageExportDirectory>(), 40);
        assert_eq!(std::mem::size_of::<Win32Tib>(), 4096);
        assert_eq!(std::mem::size_of::<DongleBypassPatch>(), 6);
    }
}


//! In-memory 32-bit PE host for Zoom ZFX DSP DLLs.
//!
//! Public surface is re-exported here so `main.rs` can keep `peldr::PeModule` paths.

mod dsp_instance;
mod hooks;
mod pe_format;
mod pe_iat;
mod pe_image;
mod pe_patches;
mod pe_reloc;
mod teb;
mod vst_host;

#[allow(unused_imports)]
pub use dsp_instance::{AudioScratch, FilterAdapter, LegacyPitchPoles, ZfxDspInstance};
#[allow(unused_imports)]
pub use hooks::is_plausible_heap_buf;
#[allow(unused_imports)]
pub use pe_format::{
    DongleBypassPatch, ImageBaseRelocationBlock, ImageDataDirectory, ImageDosHeader,
    ImageExportDirectory, ImageFileHeader, ImageImportByNameHeader, ImageImportDescriptor,
    ImageNtHeaders32, ImageOptionalHeader32, ImageSectionHeader, SpDeviceInterfaceDetailDataHeader,
    Win32CriticalSection, Win32Tib, X86JmpRel32Nop, X86MovsdDisp32, X86SubEspImm32,
    ZfxInterfaceWrapper, IMAGE_DIRECTORY_ENTRY_BASERELOC, IMAGE_DIRECTORY_ENTRY_DEBUG,
    IMAGE_DIRECTORY_ENTRY_EXCEPTION, IMAGE_DIRECTORY_ENTRY_EXPORT, IMAGE_DIRECTORY_ENTRY_IAT,
    IMAGE_DIRECTORY_ENTRY_IMPORT, IMAGE_DIRECTORY_ENTRY_RESOURCE, IMAGE_DIRECTORY_ENTRY_SECURITY,
    IMAGE_DIRECTORY_ENTRY_TLS,
};
pub use pe_image::PeModule;
#[allow(unused_imports)]
pub use teb::{init_win32_teb, Win32ContextGuard};
#[allow(unused_imports)]
pub use vst_host::{
    default_audio_master, AEffect, VstTimeInfo, VstTimeInfoFlags, CURRENT_BLOCK_SIZE,
    CURRENT_SAMPLE_RATE, CURRENT_TIME_INFO,
};

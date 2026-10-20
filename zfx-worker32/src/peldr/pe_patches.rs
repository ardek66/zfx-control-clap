//! Zoom-specific image hot-patches (dongle bypass, `Pedal_Pitch` O(N^2) skip).
use zerocopy::{little_endian as le, IntoBytes};

use super::pe_format::{DongleBypassPatch, X86JmpRel32Nop, X86SubEspImm32};
use super::pe_image::PeModule;

impl PeModule {
    /// Apply Zoom corpus image patches after map/reloc/IAT.
    ///
    /// Called from [`Self::init_zfx_plugin`] so [`Self::load`] stays generic.
    pub fn apply_zfx_image_patches(&self) {
        let base_ptr = self.base_addr.cast::<u8>();
        // SAFETY: Slicing the full mapped PE image memory to locate and patch routines.
        let mem_slice = unsafe { std::slice::from_raw_parts_mut(base_ptr, self.size) };

        // Hardware dongle check: sub esp, 0x44c → mov al,1; ret; nops
        let dongle_sig = X86SubEspImm32 {
            opcode: [0x81, 0xec],
            imm32: le::U32::new(0x044c),
        };
        let dongle_patch = DongleBypassPatch {
            mov_al_1: [0xb0, 0x01],
            ret: 0xc3,
            nops: [0x90, 0x90, 0x90],
        };
        if let Some(pos) = mem_slice
            .windows(std::mem::size_of::<X86SubEspImm32>())
            .position(|w| w == dongle_sig.as_bytes())
        {
            mem_slice[pos..pos + std::mem::size_of::<DongleBypassPatch>()]
                .copy_from_slice(dongle_patch.as_bytes());
            crate::worker_log!("[peldr] Hardware dongle check bypassed at RVA {pos:#x}");
        }

        // `Pedal_Pitch` O(N^2) envelope recalculation bypass (behavior-preserving for live pitch).
        let pitch_pedal_sig = [
            0x8bu8, 0xf3, 0xf3, 0x0f, 0x11, 0x04, 0x24, 0xe8, 0xdc, 0xed, 0xff, 0xff, 0x8b, 0x83,
            0xf4, 0x00, 0x00, 0x00,
        ];
        let pitch_pedal_patch = X86JmpRel32Nop {
            opcode: 0xe9,
            rel32: le::I32::new(0x60),
            nop: 0x90,
        };
        if let Some(pos) = mem_slice
            .windows(pitch_pedal_sig.len())
            .position(|w| w == pitch_pedal_sig)
        {
            let patch_start = pos + 12;
            let patch_len = std::mem::size_of::<X86JmpRel32Nop>();
            mem_slice[patch_start..patch_start + patch_len]
                .copy_from_slice(pitch_pedal_patch.as_bytes());
            crate::worker_log!(
                "[peldr] Pedal_Pitch O(N^2) loop hot-patched at RVA {patch_start:#x} (live pitch tracking preserved)"
            );
        }
    }
}

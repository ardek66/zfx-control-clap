//! Reverse-engineered DSP memory layout definitions and pattern matchers for Zoom ZFX.
//!
//! Parent-first model mirroring MSVC single inheritance:
//! [`DspModuleBase`] (shared prefix through [`DSP_MODULE_BASE_END`]) plus extensible
//! [`DspFamily`] relative-block rules. Leaf plugins do not get absolute offset tables;
//! constrained [`MemoryView`] scans locate family blocks inside [`FamilyScanWindow`].
//!
//! Evidence: `.rea-evidence/base_extent.json`, `*.family_evidence.json`, pilot Ghidra
//! 12.1.2 decompilation of `SetSampleRate` / ctors.

use std::ops::Range;

use zerocopy::{little_endian as le, FromBytes, Immutable, IntoBytes, KnownLayout};

/// Byte offset where the shared `CZfxDspModule` prefix ends and CritSec / family
/// fields begin. Measured across Wah / Amp / Pitch pilots (CritSec `@0xd0` or `@0xd4`).
pub const DSP_MODULE_BASE_END: usize = 0xd0;

/// `sizeof(RTL_CRITICAL_SECTION)` on Win32.
pub const CRITSEC_SIZE: usize = 0x18;

/// Harmonized Pitch Shifter-class plugins store a helper pointer at `[this+0x328]`.
/// Pitch leaf quirk — not a [`DspModuleBase`] field.
pub const HPS_HELPER_OBJECT_OFFSET: usize = 0x328;

/// Default snapshot probe length when `operator_new` size is unknown.
pub const DEFAULT_OBJECT_PROBE_LEN: usize = 0x380;

/// 5th-order cabinet / speaker / waveshaping IIR filter layout (88 bytes).
///
/// Corresponds to Zoom's `Order5FilterCoeffs` C++ structure:
/// - Feedback denominator poles: `a[0..5]` (40 bytes, positive feedback format)
/// - Feedforward numerator zeros: `b[0..6]` (48 bytes, symmetric mirror coefficients)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Order5Coeffs {
    pub a: [f64; 5],
    pub b: [f64; 6],
}

/// Zoom 5th-Order Filter Sub-Object (e.g. at channel sub-objects in amplifiers).
///
/// Layout in 32-bit C++ heap object:
/// - 0x00: vtable pointer (4 bytes)
/// - 0x04: alignment pad to 8
/// - 0x08: internal filter delay line history states `[f64; 7]` (56 bytes → ends `0x40`)
/// - 0x40: 5th-order cabinet / speaker IIR filter coefficients (88 bytes)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct Order5SubObject {
    pub vtable: le::U32,
    pub _pad_04: [u8; 4],
    pub history_states: [f64; 7],
    pub coeffs: Order5Coeffs,
}

const _: () = assert!(std::mem::size_of::<Order5SubObject>() == 0x40 + 88);

impl Order5Coeffs {
    /// Checks whether this coefficient block satisfies physical Zoom 5th-order cabinet filter invariants:
    /// - `a1` pole in `(3.0..8.0)`
    /// - `a5` stability pole in `(0.4..1.0)`
    /// - Numerator mirror symmetry: `b0 == b5`, `b1 == b4`, `b2 == b3`
    /// - Valid feedforward gain magnitude: `|b0| in (1e-9..0.01)`
    #[must_use]
    pub fn is_valid_cabinet(&self) -> bool {
        let [a1, _, _, _, a5] = self.a;
        let [b0, b1, b2, b3, b4, b5] = self.b;

        (3.0..8.0).contains(&a1)
            && (0.4..1.0).contains(&a5)
            && (b0 - b5).abs() < 1e-12
            && (b1 - b4).abs() < 1e-12
            && (b2 - b3).abs() < 1e-12
            && (1e-9..0.01).contains(&b0.abs())
    }

    /// Determines if this 48k block indicates table fallback at 96k (Zoom original code lacked 96k tables).
    #[must_use]
    pub fn is_table_fallback(&self, snap_44k: &Self, snap_96k: &Self) -> bool {
        let a1_48 = self.a[0];
        let a1_44 = snap_44k.a[0];
        let a1_96 = snap_96k.a[0];

        (a1_48 - a1_44).abs() > 1e-6
            && ((a1_96 - a1_44).abs() < 1e-4 || (a1_96 - a1_48).abs() < 1e-4)
    }
}

/// Direct Form II Transposed Biquad Filter (40 bytes).
///
/// Corresponds to Zoom's `BiquadCoeffs` C++ structure:
/// - Feedforward zeros: `b[0..3]` (24 bytes)
/// - Feedback poles: `a[0..2]` (16 bytes, stored as positive feedback values `+a1, +a2`)
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct BiquadCoeffs {
    pub b: [f64; 3],
    pub a: [f64; 2],
}

/// Typed sub-object representation for Zoom Wah / SVF Resonators.
///
/// Layout in 32-bit C++ heap object:
/// - 0x00: vtable pointer (4 bytes + 4 bytes alignment padding)
/// - 0x08: primary frequency/gain parameter `f64`
/// - 0x10: secondary bandwidth/resonance parameter `f64`
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct WahResonatorParams {
    pub vtable_pad: [u8; 8],
    pub param1: f64,
    pub param2: f64,
}

const _: () = assert!(std::mem::size_of::<WahResonatorParams>() == 24);

/// Sample-rate pair when `sr_i32` sits at offset ≡ 4 (mod 8), so `sr_f64` is contiguous.
///
/// Confirmed in `Pedal_Pitch` (`0x11c`/`0x120`) and `Auto_Wah` (`0xec`/`0xf0`) via Ghidra
/// `SetSampleRate` decompilation.
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, PartialEq)]
#[repr(C, packed)]
pub struct SampleRatePairPacked {
    pub sr_i32: i32,
    pub sr_f64: f64,
}

const _: () = assert!(std::mem::size_of::<SampleRatePairPacked>() == 12);

/// Sample-rate pair when `sr_i32` sits at offset ≡ 0 (mod 8), requiring 4 bytes of padding
/// before the 8-byte-aligned `sr_f64`.
///
/// Confirmed in `100BassMan` / `Fenderbassman100` (`sr_i32@0x218`, pad `@0x21c`, `sr_f64@0x220`).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct SampleRatePairAligned {
    pub sr_i32: i32,
    pub _pad_21c: [u8; 4],
    pub sr_f64: f64,
}

const _: () = assert!(std::mem::size_of::<SampleRatePairAligned>() == 16);

/// Shared `CZfxDspModule` prefix: vtable + early program-table / flag region through
/// [`DSP_MODULE_BASE_END`]. CritSec is a base-tail variant at `0xd0` or `0xd4`, not
/// encoded as fixed fields here (see [`CritSecKind`]).
#[derive(FromBytes, IntoBytes, KnownLayout, Immutable, Clone, Copy, Debug)]
#[repr(C)]
pub struct DspModuleBase {
    pub vtable: le::U32,
    pub _pad_04: [u8; DSP_MODULE_BASE_END - 4],
}

const _: () = assert!(std::mem::size_of::<DspModuleBase>() == DSP_MODULE_BASE_END);

/// CritSec placement variants observed in base ctors (`InitializeCriticalSection`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum CritSecKind {
    /// `unaff_EDI + 0x34` → byte `0xd0` (Wah, most Pitch, Tangerine, …).
    At0D0,
    /// `unaff_EDI + 0x35` → byte `0xd4` (100BassMan, some Amp / Distortion).
    At0D4,
    /// `unaff_EDI + 0x3a` → byte `0xe8` (Delay / some ModDelay — Evidence).
    At0E8,
    /// CritSec at byte `0x100` (Compressor ctor — Evidence).
    At0100,
    Unknown,
}

impl CritSecKind {
    #[must_use]
    pub const fn byte_offset(self) -> Option<usize> {
        match self {
            Self::At0D0 => Some(0xd0),
            Self::At0D4 => Some(0xd4),
            Self::At0E8 => Some(0xe8),
            Self::At0100 => Some(0x100),
            Self::Unknown => None,
        }
    }

    /// First byte after the CritSec object (family fields may start here).
    #[must_use]
    pub const fn family_region_start(self) -> usize {
        match self {
            Self::At0D0 => 0xd0 + CRITSEC_SIZE, // 0xe8
            Self::At0D4 => 0xd4 + CRITSEC_SIZE, // 0xec
            Self::At0E8 => 0xe8 + CRITSEC_SIZE, // 0x100
            Self::At0100 => 0x100 + CRITSEC_SIZE, // 0x118
            Self::Unknown => DSP_MODULE_BASE_END,
        }
    }
}

/// Sample-rate field discovery strategy per [`DspFamily`] (Evidence-backed).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SrDiscoveryPolicy {
    /// AmpOrder5, most Distortion: Order5 block then aligned SR pair.
    Order5ThenAligned,
    /// Wah, Pitch, Mic, ModDelay: 12-byte packed `i32`/`f64`.
    PackedPair,
    /// Cab: 16-byte aligned SR pair (not Order5-adjacent).
    AlignedPair,
    /// Utility / routing plugins: `sr_i32` at `this+0xc`.
    UtilityEarlyI32,
    /// DZ_Bert / MATCHING30 style: `f64` then `i32` at +8.
    InvertedPair,
    /// Unknown: packed → aligned → lone i32/f64, including inverted.
    FallbackScan,
}

/// Extensible family classification for relative-block scan rules.
/// New variants are added when Evidence shows a distinct relative-block pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DspFamily {
    Wah,
    AmpOrder5,
    Pitch,
    Distortion,
    Mic,
    ModDelay,
    Cab,
    Utility,
    Unknown,
}

impl DspFamily {
    /// Classify from MSVC RTTI type-descriptor strings embedded in the PE image.
    #[must_use]
    pub fn from_pe_rtti(pe_image: &[u8]) -> Self {
        const CAB_MARKERS: &[&[u8]] = &[
            b".?AVACOUSTIC370_cab",
            b".?AVAMPEG_SVTcab",
            b".?AVDiezelHerbert_cab",
            b".?AVFenderBASSMAN59_combo",
            b".?AVFernder_BM100cab",
            b".?AVHARTKE_HA3500_cab",
            b".?AVHIWATT_HC100Stack",
            b".?AVJCM800cab",
            b".?AVJazzCleancab",
            b".?AVMARSHALL_SUPERBAS_Stack",
            b".?AVMATCHLESS_DC30Combo",
            b".?AVMesadualcab",
            b".?AVOrange_GP120Stack",
            b".?AVPvStack",
            b".?AVTwinCombo",
            b".?AVUKV30_Combo",
        ];

        // Generated from .rea-evidence/rtti_family_map.json — regenerate via build_rtti_family_map.mjs
        // Order matters: Utility and specific markers before broad patterns; AmpOrder5 last.
        const RULES: &[(DspFamily, &[&[u8]])] = &[
            (
                DspFamily::Utility,
                &[
                    b".?AVInFsChk",
                    b".?AVMixer",
                    b".?AVOutFsChk",
                    b".?AVPatchLvl",
                    b".?AVPuSelect",
                    b".?AVSplitter",
                    b".?AVTuner",
                    b".?AVVUMeter",
                    b".?AVVolume",
                    b".?AVVolumePedal",
                    b".?AVinMaster",
                    b".?AVmn2ste",
                    b".?AVoutMaster",
                    b".?AVste2mn",
                ],
            ),
            (
                DspFamily::Wah,
                &[
                    b".?AVAutoWah",
                    b".?AVCry",
                    b".?AVWah_Crybaby",
                    b".?AVWah_Vox",
                ],
            ),
            (
                DspFamily::Pitch,
                &[
                    b".?AVAcoustic_Simulator",
                    b".?AVHPS",
                    b".?AVMonoPitch",
                    b".?AVOctave",
                    b".?AVPedalMonoPitch",
                    b".?AVPedalPitch",
                    b".?AVPitchShifter",
                ],
            ),
            (
                DspFamily::Distortion,
                &[
                    b".?AVBIGMUFF",
                    b".?AVBOSS_MT2",
                    b".?AVBooster",
                    b".?AVFuzzFace",
                    b".?AVMXR_Distortion",
                    b".?AVMarshall_Guvnor",
                    b".?AVOD1",
                    b".?AVProcoRat",
                    b".?AVibanezTS808",
                ],
            ),
            (
                DspFamily::Mic,
                &[
                    b".?AVAKG_C414",
                    b".?AVComp",
                    b".?AVEQ",
                    b".?AVIsolator",
                    b".?AVLimiter",
                    b".?AVMXRcomp",
                    b".?AVNEUMANN_U87",
                    b".?AVSENNHEISER_MD421",
                    b".?AVSM57",
                    b".?AVZNR",
                ],
            ),
            (
                DspFamily::ModDelay,
                &[
                    b".?AVAnalogDelay",
                    b".?AVBOSS_CE1",
                    b".?AVChorus",
                    b".?AVDelay",
                    b".?AVEnsemble",
                    b".?AVFlanger",
                    b".?AVHall",
                    b".?AVPhaser",
                    b".?AVPlate",
                    b".?AVROOM",
                    b".?AVReversDelay",
                    b".?AVRingModulator",
                    b".?AVSpring",
                    b".?AVStep",
                    b".?AVTapeEcho",
                    b".?AVTremolo",
                    b".?AVVibrato",
                ],
            ),
            (
                DspFamily::AmpOrder5,
                &[
                    b".?AVACOUSTIC370",
                    b".?AVAMPEG_SVT",
                    b".?AVDiezelHerbert",
                    b".?AVDiezelHerbertCh1",
                    b".?AVDiezelHerbertCh2",
                    b".?AVDiezelHerbertCh3",
                    b".?AVFenderBASSMAN59",
                    b".?AVFenderbassman100",
                    b".?AVHARTKE_HA3500",
                    b".?AVHIWATTCustom100",
                    b".?AVJCM800",
                    b".?AVJazzClean",
                    b".?AVMARSHALL_SUPERBASS",
                    b".?AVMATCHLESS_DC30",
                    b".?AVMATCHLESS_DC30Ch1",
                    b".?AVMATCHLESS_DC30Ch2",
                    b".?AVMarshall1959",
                    b".?AVMesaDual",
                    b".?AVMesaDualCh1",
                    b".?AVMesaDualCh2",
                    b".?AVMesaDualCh3",
                    b".?AVOrange",
                    b".?AVTwin",
                    b".?AVVOX_AC30TBX",
                    b".?AVpeavey",
                    b".?AVpeaveyCh1",
                    b".?AVpeaveyCh2",
                    b".?AVpeaveyCh3",
                ],
            ),
        ];

        for marker in CAB_MARKERS {
            if find_bytes(pe_image, marker).is_some() {
                return Self::Cab;
            }
        }
        if find_bytes(pe_image, b"_cab@@").is_some()
            || find_bytes(pe_image, b"_Cab@@").is_some()
            || find_bytes(pe_image, b"cab@@").is_some()
        {
            return Self::Cab;
        }

        for (family, markers) in RULES {
            for marker in *markers {
                if find_bytes(pe_image, marker).is_some() {
                    return *family;
                }
            }
        }
        Self::Unknown
    }

    /// Evidence-backed SR discovery order for this family.
    #[must_use]
    pub const fn sr_discovery_policy(self) -> SrDiscoveryPolicy {
        match self {
            Self::AmpOrder5 | Self::Distortion => SrDiscoveryPolicy::Order5ThenAligned,
            Self::Wah | Self::Pitch | Self::Mic | Self::ModDelay => SrDiscoveryPolicy::PackedPair,
            Self::Cab => SrDiscoveryPolicy::AlignedPair,
            Self::Utility => SrDiscoveryPolicy::UtilityEarlyI32,
            Self::Unknown => SrDiscoveryPolicy::FallbackScan,
        }
    }

    /// RTTI hint for inverted `f64`/`i32` SR layout (DZ_Bert / DiezelHerbert, MATCHING30).
    #[must_use]
    pub fn prefers_inverted_sr_from_rtti(pe_image: &[u8]) -> bool {
        const MARKERS: &[&[u8]] = &[
            b"DZ_Bert",
            b"MATCHING30",
            b".?AVDZ_Bert",
            b".?AVDiezelHerbert",
            b"DiezelHerbert",
        ];
        MARKERS
            .iter()
            .any(|m| find_bytes(pe_image, m).is_some())
    }

    /// Prefer Order5→SR adjacency discovery inside the family window.
    #[must_use]
    pub const fn prefers_order5_adjacency(self) -> bool {
        matches!(self, Self::AmpOrder5 | Self::Distortion)
    }

    /// Prefer packed 12-byte SR pair (Wah / Pitch / Mic / ModDelay).
    #[must_use]
    pub const fn prefers_packed_sr(self) -> bool {
        matches!(self, Self::Wah | Self::Pitch | Self::Mic | Self::ModDelay)
    }

    /// Distortion may use packed early SR when Order5 adjacency is absent (Booster).
    #[must_use]
    pub const fn also_try_packed_after_order5(self) -> bool {
        matches!(self, Self::Distortion)
    }
}

/// Result of family-relative sample-rate discovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SrDiscoveryHit {
    pub sr_i32_offset: Option<usize>,
    pub sr_f64_offset: Option<usize>,
    pub order5_offset: Option<usize>,
}

impl SrDiscoveryHit {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            sr_i32_offset: None,
            sr_f64_offset: None,
            order5_offset: None,
        }
    }

    #[must_use]
    pub const fn found_sr(self) -> bool {
        self.sr_i32_offset.is_some() || self.sr_f64_offset.is_some()
    }
}

/// Inclusive scan bounds for family-relative block discovery (bytes from `this`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FamilyScanWindow {
    pub start: usize,
    pub end: usize,
}

impl FamilyScanWindow {
    /// Build a window from measured `base_end`, CritSec variant, and allocated object size.
    #[must_use]
    pub fn from_object(critsec: CritSecKind, object_size: usize) -> Self {
        let start = critsec.family_region_start().max(DSP_MODULE_BASE_END);
        let end = object_size.max(start.saturating_add(1)).min(DEFAULT_OBJECT_PROBE_LEN.max(object_size));
        Self {
            start,
            end: end.max(start + 1),
        }
    }

    #[must_use]
    pub fn as_range(self) -> Range<usize> {
        self.start..self.end
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Comb filter descriptor candidate found during DSP object memory probing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CombFilterDescriptor {
    pub len_offset: usize,
    pub buf_offset: usize,
    pub base_len: u32,
    pub feedback_gain: Option<f64>,
}

/// Dynamic memory view over a byte snapshot of a C++ DSP object.
pub struct MemoryView<'a> {
    bytes: &'a [u8],
}

impl<'a> MemoryView<'a> {
    #[must_use]
    pub const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.bytes.len()
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Safely reads an unaligned value of type `T` from a byte offset using `zerocopy`.
    #[must_use]
    pub fn read_struct<T: FromBytes + KnownLayout + Immutable>(&self, offset: usize) -> Option<T> {
        let size = std::mem::size_of::<T>();
        let slice = self.bytes.get(offset..offset.checked_add(size)?)?;
        T::read_from_bytes(slice).ok()
    }

    /// Reads an `f64` value from `offset`.
    #[must_use]
    pub fn read_f64(&self, offset: usize) -> Option<f64> {
        self.read_struct::<f64>(offset)
    }

    /// Reads an `i32` value from `offset`.
    #[must_use]
    pub fn read_i32(&self, offset: usize) -> Option<i32> {
        self.read_struct::<i32>(offset)
    }

    /// Reads an `f32` value from `offset`.
    #[must_use]
    pub fn read_f32(&self, offset: usize) -> Option<f32> {
        self.read_struct::<f32>(offset)
    }

    /// Reads a `u32` value from `offset`.
    #[must_use]
    pub fn read_u32(&self, offset: usize) -> Option<u32> {
        self.read_struct::<u32>(offset)
    }

    /// Scans a range of offsets looking for a structure matching a predicate.
    pub fn find_struct<T, P>(
        &self,
        range: std::ops::Range<usize>,
        step: usize,
        predicate: P,
    ) -> Option<(usize, T)>
    where
        T: FromBytes + KnownLayout + Immutable,
        P: Fn(&T, usize) -> bool,
    {
        for off in range.step_by(step) {
            if let Some(val) = self.read_struct::<T>(off) {
                if predicate(&val, off) {
                    return Some((off, val));
                }
            }
        }
        None
    }

    /// Locates the 64-bit float sample rate field inside `window`.
    ///
    /// Pilot confirmations: `Pedal_Pitch@0x120`, `Auto_Wah@0xf0`, `100BassMan@0x220`.
    #[must_use]
    pub fn find_sr_f64_in(&self, window: Range<usize>, snap_44k: &Self) -> Option<usize> {
        let start = window.start.next_multiple_of(8);
        (start..window.end.saturating_sub(7)).step_by(8).find(|&off| {
            if let (Some(v48), Some(v44)) = (self.read_f64(off), snap_44k.read_f64(off)) {
                (v44 - 44100.0).abs() < 1e-3 && (v48 - 48000.0).abs() < 1e-3
            } else {
                false
            }
        })
    }

    /// Locates the 32-bit integer sample rate field inside `window`.
    #[must_use]
    pub fn find_sr_i32_in(&self, window: Range<usize>, snap_44k: &Self) -> Option<usize> {
        let start = window.start.next_multiple_of(4);
        (start..window.end.saturating_sub(3)).step_by(4).find(|&off| {
            if let (Some(v48), Some(v44)) = (self.read_i32(off), snap_44k.read_i32(off)) {
                v44 == 44100 && v48 == 48000
            } else {
                false
            }
        })
    }

    /// Legacy whole-object scan (pre-hierarchy); prefer [`Self::find_sr_f64_in`].
    #[must_use]
    pub fn find_sr_f64(&self, snap_44k: &Self) -> Option<usize> {
        self.find_sr_f64_in(DSP_MODULE_BASE_END..DEFAULT_OBJECT_PROBE_LEN, snap_44k)
    }

    /// Legacy whole-object scan; prefer [`Self::find_sr_i32_in`].
    #[must_use]
    pub fn find_sr_i32(&self, snap_44k: &Self) -> Option<usize> {
        self.find_sr_i32_in(DSP_MODULE_BASE_END..DEFAULT_OBJECT_PROBE_LEN, snap_44k)
    }

    /// Reads a packed `sr_i32`/`sr_f64` pair (12 bytes) at `sr_i32_offset`.
    #[must_use]
    pub fn read_sample_rate_packed(&self, sr_i32_offset: usize) -> Option<SampleRatePairPacked> {
        self.read_struct(sr_i32_offset)
    }

    /// Reads an aligned `sr_i32` + pad + `sr_f64` pair (16 bytes) at `sr_i32_offset`.
    #[must_use]
    pub fn read_sample_rate_aligned(&self, sr_i32_offset: usize) -> Option<SampleRatePairAligned> {
        self.read_struct(sr_i32_offset)
    }

    /// Finds a packed SR pair inside `window` that matches 44.1k / 48k snapshots.
    #[must_use]
    pub fn find_sr_packed_in(
        &self,
        window: Range<usize>,
        snap_44k: &Self,
    ) -> Option<(usize, SampleRatePairPacked)> {
        let start = window.start.next_multiple_of(4);
        for off in (start..window.end.saturating_sub(11)).step_by(4) {
            let Some(p48) = self.read_sample_rate_packed(off) else {
                continue;
            };
            let Some(p44) = snap_44k.read_sample_rate_packed(off) else {
                continue;
            };
            let i44 = { p44.sr_i32 };
            let i48 = { p48.sr_i32 };
            let f44 = { p44.sr_f64 };
            let f48 = { p48.sr_f64 };
            if i44 == 44100
                && i48 == 48000
                && (f44 - 44100.0).abs() < 1e-3
                && (f48 - 48000.0).abs() < 1e-3
            {
                return Some((off, p48));
            }
        }
        None
    }

    /// Finds an aligned 16-byte SR pair inside `window` matching 44.1k / 48k snapshots.
    #[must_use]
    pub fn find_sr_aligned_in(
        &self,
        window: Range<usize>,
        snap_44k: &Self,
    ) -> Option<(usize, SampleRatePairAligned)> {
        let start = window.start.next_multiple_of(8);
        for off in (start..window.end.saturating_sub(15)).step_by(8) {
            let Some(p48) = self.read_sample_rate_aligned(off) else {
                continue;
            };
            let Some(p44) = snap_44k.read_sample_rate_aligned(off) else {
                continue;
            };
            let i44 = { p44.sr_i32 };
            let i48 = { p48.sr_i32 };
            let f44 = { p44.sr_f64 };
            let f48 = { p48.sr_f64 };
            if i44 == 44100
                && i48 == 48000
                && (f44 - 44100.0).abs() < 1e-3
                && (f48 - 48000.0).abs() < 1e-3
            {
                return Some((off, p48));
            }
        }
        None
    }

    /// Finds inverted SR layout: `f64` at `f64_off`, matching `i32` at `f64_off + 8`.
    #[must_use]
    pub fn find_sr_inverted_in(
        &self,
        window: Range<usize>,
        snap_44k: &Self,
    ) -> Option<(usize, usize)> {
        let start = window.start.next_multiple_of(8);
        for f64_off in (start..window.end.saturating_sub(15)).step_by(8) {
            let i32_off = f64_off + 8;
            if let (Some(f48), Some(f44)) = (self.read_f64(f64_off), snap_44k.read_f64(f64_off)) {
                if (f44 - 44100.0).abs() < 1e-3 && (f48 - 48000.0).abs() < 1e-3 {
                    if let (Some(i48), Some(i44)) =
                        (self.read_i32(i32_off), snap_44k.read_i32(i32_off))
                    {
                        if i44 == 44100 && i48 == 48000 {
                            return Some((f64_off, i32_off));
                        }
                    }
                }
            }
        }
        None
    }

    /// Utility-family SR: `i32` at `this+0xc` (also probes `0x8..0x20`).
    ///
    /// Accepts `i32` or `f32` at the candidate offset. For the canonical `@0xc` slot, also
    /// accepts a matching `f64` at `0x10` when the dword at `0xc` is not 44100/48000 (Mixer
    /// evidence: i32-only layout at `0xc`; live snaps sometimes show float or paired f64).
    #[must_use]
    pub fn find_utility_early_sr(&self, snap_44k: &Self) -> Option<usize> {
        const UTILITY_SR_PROBE: Range<usize> = 0x8..0x20;
        if Self::utility_early_sr_at(self, snap_44k, 0xc) {
            return Some(0xc);
        }
        let start = UTILITY_SR_PROBE.start.next_multiple_of(4);
        for off in (start..UTILITY_SR_PROBE.end.saturating_sub(3)).step_by(4) {
            if off != 0xc && Self::utility_early_sr_at(self, snap_44k, off) {
                return Some(off);
            }
        }
        None
    }

    fn utility_early_sr_at(view48: &Self, view44: &Self, off: usize) -> bool {
        if let (Some(i48), Some(i44)) = (view48.read_i32(off), view44.read_i32(off)) {
            if i44 == 44100 && i48 == 48000 {
                return true;
            }
        }
        if let (Some(f48), Some(f44)) = (view48.read_f32(off), view44.read_f32(off)) {
            if (f44 - 44100.0).abs() < 1e-3 && (f48 - 48000.0).abs() < 1e-3 {
                return true;
            }
        }
        // Canonical utility slot: i32 @0xc with optional f64 @0x10 in the same snapshot pair.
        if off == 0xc {
            if let (Some(f48), Some(f44)) = (view48.read_f64(0x10), view44.read_f64(0x10)) {
                if (f44 - 44100.0).abs() < 1e-3 && (f48 - 48000.0).abs() < 1e-3 {
                    return true;
                }
            }
        }
        false
    }

    /// Run [`SrDiscoveryPolicy`] finders inside `window` (Utility ignores window for early `@0xc`).
    #[must_use]
    pub fn discover_sr(
        &self,
        policy: SrDiscoveryPolicy,
        window: Range<usize>,
        snap_44k: &Self,
        snap_96k: &Self,
        also_try_packed_after_order5: bool,
    ) -> SrDiscoveryHit {
        let mut hit = SrDiscoveryHit::empty();

        match policy {
            SrDiscoveryPolicy::Order5ThenAligned => {
                if let Some((o5, sr_i32)) =
                    self.find_order5_then_sr_aligned(window.clone(), snap_44k, snap_96k)
                {
                    hit.order5_offset = Some(o5);
                    hit.sr_i32_offset = Some(sr_i32);
                    hit.sr_f64_offset = Some(sr_i32 + 8);
                } else if also_try_packed_after_order5 {
                    if let Some((off, _)) = self.find_sr_packed_in(window.clone(), snap_44k) {
                        hit.sr_i32_offset = Some(off);
                        hit.sr_f64_offset = Some(off + 4);
                    }
                }
            }
            SrDiscoveryPolicy::PackedPair => {
                if let Some((off, _)) = self.find_sr_packed_in(window.clone(), snap_44k) {
                    hit.sr_i32_offset = Some(off);
                    hit.sr_f64_offset = Some(off + 4);
                }
            }
            SrDiscoveryPolicy::AlignedPair => {
                if let Some((off, _)) = self.find_sr_aligned_in(window.clone(), snap_44k) {
                    hit.sr_i32_offset = Some(off);
                    hit.sr_f64_offset = Some(off + 8);
                }
            }
            SrDiscoveryPolicy::UtilityEarlyI32 => {
                if let Some(off) = self.find_utility_early_sr(snap_44k) {
                    hit.sr_i32_offset = Some(off);
                }
            }
            SrDiscoveryPolicy::InvertedPair => {
                if let Some((f64_off, i32_off)) =
                    self.find_sr_inverted_in(window.clone(), snap_44k)
                {
                    hit.sr_f64_offset = Some(f64_off);
                    hit.sr_i32_offset = Some(i32_off);
                }
            }
            SrDiscoveryPolicy::FallbackScan => {
                if let Some((off, _)) = self.find_sr_packed_in(window.clone(), snap_44k) {
                    hit.sr_i32_offset = Some(off);
                    hit.sr_f64_offset = Some(off + 4);
                } else if let Some((off, _)) = self.find_sr_aligned_in(window.clone(), snap_44k)
                {
                    hit.sr_i32_offset = Some(off);
                    hit.sr_f64_offset = Some(off + 8);
                } else if let Some((f64_off, i32_off)) =
                    self.find_sr_inverted_in(window.clone(), snap_44k)
                {
                    hit.sr_f64_offset = Some(f64_off);
                    hit.sr_i32_offset = Some(i32_off);
                } else {
                    if let Some(off) = self.find_sr_f64_in(window.clone(), snap_44k) {
                        hit.sr_f64_offset = Some(off);
                    }
                    if let Some(off) = self.find_sr_i32_in(window.clone(), snap_44k) {
                        hit.sr_i32_offset = Some(off);
                    }
                }
            }
        }

        // Last-resort lone field scans when policy found nothing (except Utility, which is early-only).
        if !matches!(policy, SrDiscoveryPolicy::UtilityEarlyI32) && !hit.found_sr() {
            if let Some(off) = self.find_sr_f64_in(window.clone(), snap_44k) {
                hit.sr_f64_offset = Some(off);
            }
            if let Some(off) = self.find_sr_i32_in(window, snap_44k) {
                hit.sr_i32_offset = Some(off);
            }
        }

        hit
    }

    /// Finds valid `Order5Coeffs` where `off + 88` is an aligned SR pair matching snapshots.
    ///
    /// Explains BassMan `Order5@0x1c0` / `sr@0x218` and Tangerine `0x1d0` / `0x228` with one rule.
    #[must_use]
    pub fn find_order5_then_sr_aligned(
        &self,
        window: Range<usize>,
        snap_44k: &Self,
        snap_96k: &Self,
    ) -> Option<(usize /* order5 */, usize /* sr_i32 */)> {
        let o5_size = std::mem::size_of::<Order5Coeffs>();
        let start = window.start.next_multiple_of(8);
        for off in (start..window.end.saturating_sub(o5_size + 15)).step_by(8) {
            let Some(c48) = self.read_struct::<Order5Coeffs>(off) else {
                continue;
            };
            let Some(c44) = snap_44k.read_struct::<Order5Coeffs>(off) else {
                continue;
            };
            let Some(c96) = snap_96k.read_struct::<Order5Coeffs>(off) else {
                continue;
            };
            if !c48.is_valid_cabinet() || !c48.is_table_fallback(&c44, &c96) {
                continue;
            }
            let sr_off = off + o5_size;
            let Some(p48) = self.read_sample_rate_aligned(sr_off) else {
                continue;
            };
            let Some(p44) = snap_44k.read_sample_rate_aligned(sr_off) else {
                continue;
            };
            if p44.sr_i32 == 44100
                && p48.sr_i32 == 48000
                && (p44.sr_f64 - 44100.0).abs() < 1e-3
                && (p48.sr_f64 - 48000.0).abs() < 1e-3
            {
                return Some((off, sr_off));
            }
        }
        None
    }

    /// Wah multiplier inside `window` (48k branch scalar ~0.12..0.16).
    #[must_use]
    pub fn find_wah_mult_in(
        &self,
        window: Range<usize>,
        snap_44k: &Self,
    ) -> Option<(usize, f64)> {
        let start = window.start.next_multiple_of(8);
        for off in (start..window.end.saturating_sub(7)).step_by(8) {
            if let (Some(v48), Some(v44)) = (self.read_f64(off), snap_44k.read_f64(off)) {
                if (0.12..0.16).contains(&v48)
                    && v44.mul_add(44100.0, -(v48 * 48000.0)).abs() < 50.0
                {
                    return Some((off, v48));
                }
            }
        }
        None
    }

    /// Locates a comb delay filter length, buffer pointer, and feedback gain.
    #[must_use]
    pub fn find_comb_filter<F>(
        &self,
        snap_44k: &Self,
        is_plausible_heap_buf: F,
        skip_f64_offset: Option<usize>,
        skip_i32_offset: Option<usize>,
    ) -> Option<CombFilterDescriptor>
    where
        F: Fn(usize) -> bool,
    {
        for off in (0x200..0x2a0).step_by(4) {
            if let Some(f64_off) = skip_f64_offset {
                if off >= f64_off && off < f64_off + 8 {
                    continue;
                }
            }
            if let Some(i32_off) = skip_i32_offset {
                if off >= i32_off && off < i32_off + 4 {
                    continue;
                }
            }
            if let (Some(v48), Some(v44)) = (self.read_u32(off), snap_44k.read_u32(off)) {
                if v44 < 1000
                    && v48 < 1000
                    && v44 > 0
                    && v48 > v44
                    && (f64::from(v44) * 48000.0 / 44100.0 - f64::from(v48)).abs() <= 1.0
                {
                    let cand_buf_10 = self
                        .read_u32(off + 0x10)
                        .map_or(0, |p| usize::try_from(p).unwrap_or(0));
                    let cand_buf_8 = self
                        .read_u32(off + 0x8)
                        .map_or(0, |p| usize::try_from(p).unwrap_or(0));

                    let buf_off = match () {
                        () if is_plausible_heap_buf(cand_buf_10) => Some(off + 0x10),
                        () if is_plausible_heap_buf(cand_buf_8) => Some(off + 0x8),
                        () => None,
                    };

                    if let Some(bo) = buf_off {
                        let feedback_gain = match off {
                            8.. => self.read_f64(off - 8).filter(|&g| (0.5..0.99).contains(&g)),
                            _ => None,
                        };

                        return Some(CombFilterDescriptor {
                            len_offset: off,
                            buf_offset: bo,
                            base_len: v48,
                            feedback_gain,
                        });
                    }
                }
            }
        }
        None
    }

    /// Locates the 5th-order cabinet filter inside `window`.
    #[must_use]
    pub fn find_order5_in(
        &self,
        window: Range<usize>,
        snap_44k: &Self,
        snap_96k: &Self,
    ) -> Option<(usize, Order5Coeffs)> {
        let start = window.start.next_multiple_of(8);
        self.find_struct::<Order5Coeffs, _>(start..window.end, 8, |c48, off| {
            if let (Some(c44), Some(c96)) = (
                snap_44k.read_struct::<Order5Coeffs>(off),
                snap_96k.read_struct::<Order5Coeffs>(off),
            ) {
                c48.is_valid_cabinet() && c48.is_table_fallback(&c44, &c96)
            } else {
                false
            }
        })
    }

    /// Locates the 5th-order cabinet filter (default family window).
    #[must_use]
    pub fn find_order5(&self, snap_44k: &Self, snap_96k: &Self) -> Option<(usize, Order5Coeffs)> {
        self.find_order5_in(0x180..0x260, snap_44k, snap_96k)
    }

    /// Locates the wah multiplier field (default wah-tail window).
    #[must_use]
    pub fn find_wah_multiplier(&self, snap_44k: &Self) -> Option<(usize, f64)> {
        self.find_wah_mult_in(0x180..0x240, snap_44k)
    }
}

const _: () = assert!(std::mem::size_of::<Order5Coeffs>() == 88);
const _: () = assert!(std::mem::size_of::<BiquadCoeffs>() == 40);
// BassMan / Tangerine relative adjacency: Order5 (88) immediately precedes aligned SR.
const _: () = assert!(0x1c0 + 88 == 0x218);
const _: () = assert!(0x1d0 + 88 == 0x228);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order5_coeffs_size_and_invariants() {
        assert_eq!(std::mem::size_of::<Order5Coeffs>(), 88);

        // Typical Zoom cabinet coefficients (e.g. from Great_Muff / Fuzz_Smile)
        let sample = Order5Coeffs {
            a: [4.4532, -9.321, 9.214, -4.512, 0.8841],
            b: [0.00012, 0.00060, 0.00120, 0.00120, 0.00060, 0.00012],
        };
        assert!(sample.is_valid_cabinet());

        let invalid = Order5Coeffs {
            a: [2.1, 0.0, 0.0, 0.0, 0.2], // a1 < 3.0, a5 < 0.4
            b: [0.00012, 0.0, 0.0, 0.0, 0.0, 0.00012],
        };
        assert!(!invalid.is_valid_cabinet());
    }

    #[test]
    fn test_memory_view_read_struct() {
        let mut buf = [0u8; 128];
        let val_f64 = 48000.0f64;
        buf[16..24].copy_from_slice(&val_f64.to_le_bytes());

        let view = MemoryView::new(&buf);
        assert_eq!(view.read_f64(16), Some(48000.0));
        assert_eq!(view.read_f64(0), Some(0.0));
        assert_eq!(view.read_f64(124), None); // Out of bounds
    }

    #[test]
    fn test_memory_view_find_sr() {
        // Legacy find_sr_* scans from DSP_MODULE_BASE_END; place values in family region.
        let mut snap44 = [0u8; 0x200];
        let mut snap48 = [0u8; 0x200];
        let f64_off = 0xe0;
        let i32_off = 0xf0;

        snap44[f64_off..f64_off + 8].copy_from_slice(&44100.0f64.to_le_bytes());
        snap48[f64_off..f64_off + 8].copy_from_slice(&48000.0f64.to_le_bytes());
        snap44[i32_off..i32_off + 4].copy_from_slice(&44100i32.to_le_bytes());
        snap48[i32_off..i32_off + 4].copy_from_slice(&48000i32.to_le_bytes());

        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);

        assert_eq!(v48.find_sr_f64(&v44), Some(f64_off));
        assert_eq!(v48.find_sr_i32(&v44), Some(i32_off));
    }

    #[test]
    fn test_sample_rate_pair_packed_roundtrip() {
        let mut buf = [0u8; 32];
        // Mimic Pedal_Pitch: i32 @ 0x04 (≡4 mod 8), f64 @ 0x08
        buf[4..8].copy_from_slice(&48000i32.to_le_bytes());
        buf[8..16].copy_from_slice(&48000.0f64.to_le_bytes());
        let view = MemoryView::new(&buf);
        let pair = view.read_sample_rate_packed(4).unwrap();
        let sr_i32 = { pair.sr_i32 };
        let sr_f64 = { pair.sr_f64 };
        assert_eq!(sr_i32, 48000);
        assert_eq!(sr_f64, 48000.0);
    }

    #[test]
    fn test_sample_rate_pair_aligned_roundtrip() {
        let mut buf = [0u8; 32];
        // Mimic BassMan: i32 @ 0x00, pad, f64 @ 0x08
        buf[0..4].copy_from_slice(&48000i32.to_le_bytes());
        buf[8..16].copy_from_slice(&48000.0f64.to_le_bytes());
        let view = MemoryView::new(&buf);
        let pair = view.read_sample_rate_aligned(0).unwrap();
        assert_eq!({ pair.sr_i32 }, 48000);
        assert_eq!({ pair.sr_f64 }, 48000.0);
    }

    #[test]
    fn test_family_rtti_classify_wah() {
        let mut pe = vec![0u8; 64];
        let marker = b".?AVAutoWah@@";
        pe[10..10 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::Wah);
    }

    #[test]
    fn test_family_rtti_classify_amp() {
        let mut pe = vec![0u8; 64];
        let marker = b".?AVOrange@@";
        pe[10..10 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::AmpOrder5);
    }

    #[test]
    fn test_family_rtti_mic_substring_does_not_steal_amp() {
        let mut pe = vec![0u8; 256];
        pe[4..7].copy_from_slice(b"Mic");
        let amp = b".?AVFenderbassman100@@";
        pe[40..40 + amp.len()].copy_from_slice(amp);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::AmpOrder5);
    }

    #[test]
    fn test_family_rtti_classify_mic_akg_c414() {
        let mut pe = vec![0u8; 64];
        let marker = b".?AVAKG_C414@@";
        pe[8..8 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::Mic);
    }

    #[test]
    fn test_family_rtti_classify_marshall1959_amp() {
        let mut pe = vec![0u8; 128];
        let marker = b".?AVMarshall1959@@";
        pe[16..16 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::AmpOrder5);
        assert_ne!(
            DspFamily::from_pe_rtti(&pe),
            DspFamily::Unknown,
            "Marshall1959 must not rely on too-short .?AV1959 substring"
        );
    }

    #[test]
    fn test_family_rtti_classify_diezel_herbert_amp() {
        let mut pe = vec![0u8; 128];
        let marker = b".?AVDiezelHerbert@@";
        pe[12..12 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::AmpOrder5);
        assert!(DspFamily::prefers_inverted_sr_from_rtti(&pe));
    }

    #[test]
    fn test_family_rtti_classify_dz_bert_plain_name_inverted_sr_only() {
        let mut pe = vec![0u8; 64];
        let marker = b"DZ_Bert";
        pe[20..20 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::Unknown);
        assert!(DspFamily::prefers_inverted_sr_from_rtti(&pe));
    }

    #[test]
    fn test_family_rtti_classify_compressor_mic() {
        let mut pe = vec![0u8; 128];
        let marker = b".?AVComp@@";
        pe[8..8 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::Mic);
    }

    #[test]
    fn test_family_rtti_classify_mxrcomp_mic() {
        let mut pe = vec![0u8; 128];
        let marker = b".?AVMXRcomp@@";
        pe[8..8 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::Mic);
    }

    #[test]
    fn test_family_rtti_classify_classic_amp_ampeg_svt() {
        let mut pe = vec![0u8; 128];
        let marker = b".?AVAMPEG_SVT@@";
        pe[16..16 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::AmpOrder5);
    }

    fn pe_with_marker(marker: &[u8]) -> Vec<u8> {
        let mut pe = vec![0u8; marker.len() + 32];
        pe[16..16 + marker.len()].copy_from_slice(marker);
        pe
    }

    #[test]
    fn test_family_rtti_evidence_markers_by_family() {
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVAnalogDelay@@")),
            DspFamily::ModDelay
        );
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVOD1@@")),
            DspFamily::Distortion
        );
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVBIGMUFF@@")),
            DspFamily::Distortion
        );
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVBOSS_CE1@@")),
            DspFamily::ModDelay
        );
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVMonoPitch@@")),
            DspFamily::Pitch
        );
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVPatchLvl@@")),
            DspFamily::Utility
        );
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVACOUSTIC370_cab@@")),
            DspFamily::Cab
        );
        assert_eq!(
            DspFamily::from_pe_rtti(&pe_with_marker(b".?AVACOUSTIC370@@")),
            DspFamily::AmpOrder5
        );
    }

    #[test]
    fn test_order5_then_sr_adjacency_finder() {
        let mut snap44 = vec![0u8; 0x280];
        let mut snap48 = vec![0u8; 0x280];
        let mut snap96 = vec![0u8; 0x280];
        let order5_off = 0x1c0;
        let sr_off = order5_off + 88;

        let c44 = Order5Coeffs {
            a: [4.0, -8.0, 8.0, -4.0, 0.9],
            b: [0.0001, 0.0005, 0.001, 0.001, 0.0005, 0.0001],
        };
        let c48 = Order5Coeffs {
            a: [5.0, -9.0, 9.0, -4.5, 0.85],
            b: c44.b,
        };
        // 96k falls back to 44.1k table
        let c96 = c44;

        snap44[order5_off..order5_off + 88].copy_from_slice(c44.as_bytes());
        snap48[order5_off..order5_off + 88].copy_from_slice(c48.as_bytes());
        snap96[order5_off..order5_off + 88].copy_from_slice(c96.as_bytes());

        snap44[sr_off..sr_off + 4].copy_from_slice(&44100i32.to_le_bytes());
        snap48[sr_off..sr_off + 4].copy_from_slice(&48000i32.to_le_bytes());
        snap44[sr_off + 8..sr_off + 16].copy_from_slice(&44100.0f64.to_le_bytes());
        snap48[sr_off + 8..sr_off + 16].copy_from_slice(&48000.0f64.to_le_bytes());

        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);
        let v96 = MemoryView::new(&snap96);
        let found = v48
            .find_order5_then_sr_aligned(0x180..0x260, &v44, &v96)
            .expect("adjacency");
        assert_eq!(found, (order5_off, sr_off));
    }

    #[test]
    fn test_family_scan_window_after_critsec() {
        assert_eq!(
            FamilyScanWindow::from_object(CritSecKind::At0D0, 0x250).start,
            0xe8
        );
        assert_eq!(
            FamilyScanWindow::from_object(CritSecKind::At0D4, 0x250).start,
            0xec
        );
        assert_eq!(
            FamilyScanWindow::from_object(CritSecKind::At0E8, 0x300).start,
            0x100
        );
        assert_eq!(
            FamilyScanWindow::from_object(CritSecKind::At0100, 0x300).start,
            0x118
        );
        assert_eq!(
            FamilyScanWindow::from_object(CritSecKind::Unknown, 0x200).start,
            DSP_MODULE_BASE_END
        );
    }

    #[test]
    fn test_find_sr_packed_in_window() {
        let mut snap44 = vec![0u8; 0x120];
        let mut snap48 = vec![0u8; 0x120];
        let off = 0xf0;
        snap44[off..off + 4].copy_from_slice(&44100i32.to_le_bytes());
        snap48[off..off + 4].copy_from_slice(&48000i32.to_le_bytes());
        snap44[off + 4..off + 12].copy_from_slice(&44100.0f64.to_le_bytes());
        snap48[off + 4..off + 12].copy_from_slice(&48000.0f64.to_le_bytes());
        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);
        let found = v48
            .find_sr_packed_in(0xe8..0x110, &v44)
            .expect("packed pair");
        assert_eq!(found.0, off);
    }

    #[test]
    fn test_find_sr_aligned_in_window() {
        let mut snap44 = vec![0u8; 0x230];
        let mut snap48 = vec![0u8; 0x230];
        let off = 0x218;
        snap44[off..off + 4].copy_from_slice(&44100i32.to_le_bytes());
        snap48[off..off + 4].copy_from_slice(&48000i32.to_le_bytes());
        snap44[off + 8..off + 16].copy_from_slice(&44100.0f64.to_le_bytes());
        snap48[off + 8..off + 16].copy_from_slice(&48000.0f64.to_le_bytes());
        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);
        let found = v48
            .find_sr_aligned_in(0x200..0x240, &v44)
            .expect("aligned pair");
        assert_eq!(found.0, off);
    }

    #[test]
    fn test_find_sr_inverted_in_window() {
        let mut snap44 = vec![0u8; 0x200];
        let mut snap48 = vec![0u8; 0x200];
        let f64_off = 0x180;
        let i32_off = f64_off + 8;
        snap44[f64_off..f64_off + 8].copy_from_slice(&44100.0f64.to_le_bytes());
        snap48[f64_off..f64_off + 8].copy_from_slice(&48000.0f64.to_le_bytes());
        snap44[i32_off..i32_off + 4].copy_from_slice(&44100i32.to_le_bytes());
        snap48[i32_off..i32_off + 4].copy_from_slice(&48000i32.to_le_bytes());
        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);
        assert_eq!(
            v48.find_sr_inverted_in(0x160..0x1a0, &v44),
            Some((f64_off, i32_off))
        );
    }

    #[test]
    fn test_find_utility_early_sr() {
        let mut snap44 = [0u8; 0x30];
        let mut snap48 = [0u8; 0x30];
        snap44[0xc..0x10].copy_from_slice(&44100i32.to_le_bytes());
        snap48[0xc..0x10].copy_from_slice(&48000i32.to_le_bytes());
        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);
        assert_eq!(v48.find_utility_early_sr(&v44), Some(0xc));
    }

    #[test]
    fn test_find_utility_early_sr_f32_at_0xc() {
        let mut snap44 = [0u8; 0x30];
        let mut snap48 = [0u8; 0x30];
        snap44[0xc..0x10].copy_from_slice(&44100.0f32.to_le_bytes());
        snap48[0xc..0x10].copy_from_slice(&48000.0f32.to_le_bytes());
        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);
        assert_eq!(v48.find_utility_early_sr(&v44), Some(0xc));
    }

    #[test]
    fn test_find_utility_early_sr_f64_at_0x10() {
        let mut snap44 = [0u8; 0x30];
        let mut snap48 = [0u8; 0x30];
        snap44[0x10..0x18].copy_from_slice(&44100.0f64.to_le_bytes());
        snap48[0x10..0x18].copy_from_slice(&48000.0f64.to_le_bytes());
        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);
        assert_eq!(v48.find_utility_early_sr(&v44), Some(0xc));
    }

    #[test]
    fn test_sr_discovery_policy_and_prefers() {
        assert_eq!(
            DspFamily::Cab.sr_discovery_policy(),
            SrDiscoveryPolicy::AlignedPair
        );
        assert_eq!(
            DspFamily::Utility.sr_discovery_policy(),
            SrDiscoveryPolicy::UtilityEarlyI32
        );
        assert_eq!(
            DspFamily::AmpOrder5.sr_discovery_policy(),
            SrDiscoveryPolicy::Order5ThenAligned
        );
        assert!(!DspFamily::Cab.prefers_order5_adjacency());
        assert!(DspFamily::AmpOrder5.prefers_order5_adjacency());
        assert!(DspFamily::Mic.prefers_packed_sr());
        assert!(!DspFamily::Cab.prefers_packed_sr());
    }

    #[test]
    fn test_family_rtti_utility_and_acoustic() {
        let mut pe = vec![0u8; 128];
        let marker = b".?AVMixer@@";
        pe[8..8 + marker.len()].copy_from_slice(marker);
        assert_eq!(DspFamily::from_pe_rtti(&pe), DspFamily::Utility);

        let mut pe2 = vec![0u8; 128];
        let ac = b".?AVAcoustic_Simulator@@";
        pe2[20..20 + ac.len()].copy_from_slice(ac);
        assert_eq!(DspFamily::from_pe_rtti(&pe2), DspFamily::Pitch);
    }

    #[test]
    fn test_default_object_probe_len_covers_hps() {
        assert!(DEFAULT_OBJECT_PROBE_LEN >= HPS_HELPER_OBJECT_OFFSET + 8);
    }

    /// Smoke: fixture JSON from R3 lists the plan's loader-family reps.
    #[test]
    fn test_loader_family_expectations_file_lists_smoke_reps() {
        let raw = include_str!("../tests/loader_family_expectations.json");
        for dll in [
            "AC_Bass_370_Stack_1x18",
            "100BassMan",
            "Auto_Wah",
            "Mixer",
            "DZ_Bert",
            "HPS",
            "Delay",
            "Compressor",
        ] {
            assert!(
                raw.contains(dll),
                "expectations JSON missing smoke rep {dll}"
            );
        }
        assert!(raw.contains("CabCompactSr"));
        assert!(raw.contains("UtilityEarlySr"));
        assert!(raw.contains("InvertedSr"));
        assert!(raw.contains("HpsBody"));
    }

    fn write_i32(buf: &mut [u8], off: usize, v: i32) {
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    fn write_f64(buf: &mut [u8], off: usize, v: f64) {
        buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
    }

    /// Smoke: discover_sr recovers Evidence offsets for one rep per packing style.
    #[test]
    fn test_discover_sr_smoke_loader_family_reps() {
        const LEN: usize = 0x380;

        // CabCompactSr / AC_Bass — aligned @0xd0/0xd8
        {
            let mut s44 = vec![0u8; LEN];
            let mut s48 = vec![0u8; LEN];
            let s96 = vec![0u8; LEN];
            write_i32(&mut s44, 0xd0, 44100);
            write_i32(&mut s48, 0xd0, 48000);
            write_f64(&mut s44, 0xd8, 44100.0);
            write_f64(&mut s48, 0xd8, 48000.0);
            let hit = MemoryView::new(&s48).discover_sr(
                SrDiscoveryPolicy::AlignedPair,
                0xc0..LEN,
                &MemoryView::new(&s44),
                &MemoryView::new(&s96),
                false,
            );
            assert_eq!(hit.sr_i32_offset, Some(0xd0));
            assert_eq!(hit.sr_f64_offset, Some(0xd8));
        }

        // PackedSr / Auto_Wah — packed @0xec/0xf0
        {
            let mut s44 = vec![0u8; LEN];
            let mut s48 = vec![0u8; LEN];
            let s96 = vec![0u8; LEN];
            write_i32(&mut s44, 0xec, 44100);
            write_i32(&mut s48, 0xec, 48000);
            write_f64(&mut s44, 0xf0, 44100.0);
            write_f64(&mut s48, 0xf0, 48000.0);
            let hit = MemoryView::new(&s48).discover_sr(
                SrDiscoveryPolicy::PackedPair,
                0xe8..LEN,
                &MemoryView::new(&s44),
                &MemoryView::new(&s96),
                false,
            );
            assert_eq!(hit.sr_i32_offset, Some(0xec));
            assert_eq!(hit.sr_f64_offset, Some(0xf0));
        }

        // UtilityEarlySr / Mixer — i32 @0xc
        {
            let mut s44 = vec![0u8; LEN];
            let mut s48 = vec![0u8; LEN];
            let s96 = vec![0u8; LEN];
            write_i32(&mut s44, 0xc, 44100);
            write_i32(&mut s48, 0xc, 48000);
            let hit = MemoryView::new(&s48).discover_sr(
                SrDiscoveryPolicy::UtilityEarlyI32,
                0..LEN,
                &MemoryView::new(&s44),
                &MemoryView::new(&s96),
                false,
            );
            assert_eq!(hit.sr_i32_offset, Some(0xc));
        }

        // InvertedSr / DZ_Bert — f64@0xf0 then i32@0xf8
        {
            let mut s44 = vec![0u8; LEN];
            let mut s48 = vec![0u8; LEN];
            let s96 = vec![0u8; LEN];
            write_f64(&mut s44, 0xf0, 44100.0);
            write_f64(&mut s48, 0xf0, 48000.0);
            write_i32(&mut s44, 0xf8, 44100);
            write_i32(&mut s48, 0xf8, 48000);
            let hit = MemoryView::new(&s48).discover_sr(
                SrDiscoveryPolicy::InvertedPair,
                0xe8..LEN,
                &MemoryView::new(&s44),
                &MemoryView::new(&s96),
                false,
            );
            assert_eq!(hit.sr_f64_offset, Some(0xf0));
            assert_eq!(hit.sr_i32_offset, Some(0xf8));
        }

        // Order5ThenSrAligned / BassMan-style adjacency @0x1c0 → sr@0x218
        {
            let mut s44 = vec![0u8; LEN];
            let mut s48 = vec![0u8; LEN];
            let mut s96 = vec![0u8; LEN];
            let order5_off = 0x1c0;
            let sr_off = order5_off + 88;
            let c44 = Order5Coeffs {
                a: [4.0, -8.0, 8.0, -4.0, 0.9],
                b: [0.0001, 0.0005, 0.001, 0.001, 0.0005, 0.0001],
            };
            let c48 = Order5Coeffs {
                a: [5.0, -9.0, 9.0, -4.5, 0.85],
                b: c44.b,
            };
            s44[order5_off..order5_off + 88].copy_from_slice(c44.as_bytes());
            s48[order5_off..order5_off + 88].copy_from_slice(c48.as_bytes());
            s96[order5_off..order5_off + 88].copy_from_slice(c44.as_bytes());
            write_i32(&mut s44, sr_off, 44100);
            write_i32(&mut s48, sr_off, 48000);
            write_f64(&mut s44, sr_off + 8, 44100.0);
            write_f64(&mut s48, sr_off + 8, 48000.0);
            let hit = MemoryView::new(&s48).discover_sr(
                SrDiscoveryPolicy::Order5ThenAligned,
                0x180..LEN,
                &MemoryView::new(&s44),
                &MemoryView::new(&s96),
                false,
            );
            assert_eq!(hit.order5_offset, Some(order5_off));
            assert_eq!(hit.sr_i32_offset, Some(sr_off));
            assert_eq!(hit.sr_f64_offset, Some(sr_off + 8));
        }

        // Distortion packed fallback after failed Order5 (Booster)
        {
            let mut s44 = vec![0u8; LEN];
            let mut s48 = vec![0u8; LEN];
            let s96 = vec![0u8; LEN];
            write_i32(&mut s44, 0xec, 44100);
            write_i32(&mut s48, 0xec, 48000);
            write_f64(&mut s44, 0xf0, 44100.0);
            write_f64(&mut s48, 0xf0, 48000.0);
            let hit = MemoryView::new(&s48).discover_sr(
                SrDiscoveryPolicy::Order5ThenAligned,
                0xe8..LEN,
                &MemoryView::new(&s44),
                &MemoryView::new(&s96),
                true, // also_try_packed_after_order5
            );
            assert_eq!(hit.sr_i32_offset, Some(0xec));
            assert_eq!(hit.sr_f64_offset, Some(0xf0));
        }

        // HPS helper offset remains in probe range
        assert!(HPS_HELPER_OBJECT_OFFSET < LEN);
        assert_eq!(DspFamily::Pitch.sr_discovery_policy(), SrDiscoveryPolicy::PackedPair);
    }

}

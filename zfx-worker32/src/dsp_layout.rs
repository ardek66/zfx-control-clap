//! Reverse-engineered DSP memory layout definitions and pattern matchers for Zoom ZFX.
//!
//! Provides type-safe representations of C++ filter coefficient blocks,
//! circular delay descriptors, and sample-rate adapter state using `zerocopy`.

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

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

    /// Locates the 64-bit float sample rate field where 44.1k and 48k match within tolerance.
    #[must_use]
    pub fn find_sr_f64(&self, snap_44k: &Self) -> Option<usize> {
        (0x40..0x2f8).step_by(8).find(|&off| {
            if let (Some(v48), Some(v44)) = (self.read_f64(off), snap_44k.read_f64(off)) {
                (v44 - 44100.0).abs() < 1e-3 && (v48 - 48000.0).abs() < 1e-3
            } else {
                false
            }
        })
    }

    /// Locates the 32-bit integer sample rate field where 44.1k and 48k match exactly.
    #[must_use]
    pub fn find_sr_i32(&self, snap_44k: &Self) -> Option<usize> {
        (0x40..0x2fc).step_by(4).find(|&off| {
            if let (Some(v48), Some(v44)) = (self.read_i32(off), snap_44k.read_i32(off)) {
                v44 == 44100 && v48 == 48000
            } else {
                false
            }
        })
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

                    let buf_off = if is_plausible_heap_buf(cand_buf_10) {
                        Some(off + 0x10)
                    } else if is_plausible_heap_buf(cand_buf_8) {
                        Some(off + 0x8)
                    } else {
                        None
                    };

                    if let Some(bo) = buf_off {
                        let feedback_gain = if off >= 8 {
                            self.read_f64(off - 8).filter(|&g| (0.5..0.99).contains(&g))
                        } else {
                            None
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

    /// Locates the 5th-order cabinet filter.
    #[must_use]
    pub fn find_order5(&self, snap_44k: &Self, snap_96k: &Self) -> Option<(usize, Order5Coeffs)> {
        self.find_struct::<Order5Coeffs, _>(0x180..0x260, 8, |c48, off| {
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

    /// Locates the wah multiplier field.
    #[must_use]
    pub fn find_wah_multiplier(&self, snap_44k: &Self) -> Option<(usize, f64)> {
        for off in (0x180..0x240).step_by(8) {
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
}

const _: () = assert!(std::mem::size_of::<Order5Coeffs>() == 88);
const _: () = assert!(std::mem::size_of::<BiquadCoeffs>() == 40);

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
        let mut snap44 = [0u8; 256];
        let mut snap48 = [0u8; 256];

        44100.0f64
            .to_le_bytes()
            .iter()
            .enumerate()
            .for_each(|(i, &b)| snap44[0x80 + i] = b);
        48000.0f64
            .to_le_bytes()
            .iter()
            .enumerate()
            .for_each(|(i, &b)| snap48[0x80 + i] = b);

        44100i32
            .to_le_bytes()
            .iter()
            .enumerate()
            .for_each(|(i, &b)| snap44[0x90 + i] = b);
        48000i32
            .to_le_bytes()
            .iter()
            .enumerate()
            .for_each(|(i, &b)| snap48[0x90 + i] = b);

        let v44 = MemoryView::new(&snap44);
        let v48 = MemoryView::new(&snap48);

        assert_eq!(v48.find_sr_f64(&v44), Some(0x80));
        assert_eq!(v48.find_sr_i32(&v44), Some(0x90));
    }
}

//! Mathematical engine for analog prototype extraction and continuous-domain
//! bilinear transform (BLT) re-discretization of Zoom ZFX IIR filters.
//!
//! Provides sample-rate-invariant digital biquad coefficient computation by
//! recovering the continuous transfer function $H(s)$ from reference coefficients
//! (e.g. 48.0 kHz) and re-discretizing at arbitrary host sample rates.

#![allow(clippy::suboptimal_flops)]
#![allow(clippy::doc_markdown)]

/// Analog prototype of a 2nd-order section (biquad).
///
/// Continuous-domain transfer function:
/// $$H(s) = \frac{\beta_2 s^2 + \beta_1 s + \beta_0}{\alpha_2 s^2 + \alpha_1 s + \alpha_0}$$
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AnalogBiquad {
    /// Numerator coefficients $[\beta_0, \beta_1, \beta_2]$.
    pub beta: [f64; 3],
    /// Denominator coefficients $[\alpha_0, \alpha_1, \alpha_2]$.
    pub alpha: [f64; 3],
}

/// Analog prototype of a 1st-order section (single pole / single zero).
///
/// Continuous-domain transfer function:
/// $$H(s) = \frac{\beta_1 s + \beta_0}{\alpha_1 s + \alpha_0}$$
#[derive(Clone, Copy, Debug, PartialEq)]
#[allow(dead_code)]
pub struct AnalogFirstOrder {
    /// Numerator coefficients $[\beta_0, \beta_1]$.
    pub beta: [f64; 2],
    /// Denominator coefficients $[\alpha_0, \alpha_1]$.
    pub alpha: [f64; 2],
}

impl AnalogBiquad {
    /// Extracts the continuous analog prototype $H(s)$ from digital biquad
    /// coefficients at a known reference sample rate $f_{s,\text{ref}}$.
    ///
    /// # Arguments
    /// * `b` - Digital numerator coefficients $[b_0, b_1, b_2]$.
    /// * `a` - Digital denominator coefficients $[a_0, a_1, a_2]$ (typically $a_0 = 1.0$).
    /// * `fs_ref` - Reference sample rate in Hz (e.g. 48000.0).
    #[must_use]
    pub fn from_digital(b: [f64; 3], a: [f64; 3], fs_ref: f64) -> Self {
        let k = 2.0 * fs_ref;
        let k2 = k * k;

        // Exact inverse bilinear transform formulas derived from M(K)^(-1):
        // c0 = 0.25 * (p0 + p1 + p2)
        // c1 = 0.5 / K * (p0 - p2)
        // c2 = 0.25 / K^2 * (p0 - p1 + p2)
        let beta = [
            0.25 * (b[0] + b[1] + b[2]),
            (0.5 / k) * (b[0] - b[2]),
            (0.25 / k2) * (b[0] - b[1] + b[2]),
        ];

        let alpha = [
            0.25 * (a[0] + a[1] + a[2]),
            (0.5 / k) * (a[0] - a[2]),
            (0.25 / k2) * (a[0] - a[1] + a[2]),
        ];

        Self { beta, alpha }
    }

    /// Discretizes the analog prototype $H(s)$ into digital biquad coefficients
    /// at the target sample rate $f_{s,\text{target}}$ using the bilinear transform.
    ///
    /// Returns $([b_0, b_1, b_2], [1.0, a_1, a_2])$ normalized such that $a_0 = 1.0$.
    #[must_use]
    pub fn to_digital(self, fs_target: f64) -> ([f64; 3], [f64; 3]) {
        let k = 2.0 * fs_target;
        let k2 = k * k;

        // Forward bilinear transform formulas derived from M(K):
        // p0 = c0 + c1*K + c2*K^2
        // p1 = 2*c0 - 2*c2*K^2
        // p2 = c0 - c1*K + c2*K^2
        let b0_raw = self.beta[0] + (self.beta[1] * k) + (self.beta[2] * k2);
        let b1_raw = 2.0 * (self.beta[0] - (self.beta[2] * k2));
        let b2_raw = (self.beta[0] - (self.beta[1] * k)) + (self.beta[2] * k2);

        let a0_raw = self.alpha[0] + (self.alpha[1] * k) + (self.alpha[2] * k2);
        let a1_raw = 2.0 * (self.alpha[0] - (self.alpha[2] * k2));
        let a2_raw = (self.alpha[0] - (self.alpha[1] * k)) + (self.alpha[2] * k2);

        if a0_raw.abs() < 1e-15 {
            // Degenerate denominator fallback to avoid division by zero
            return ([1.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        }

        let inv_a0 = 1.0 / a0_raw;
        (
            [b0_raw * inv_a0, b1_raw * inv_a0, b2_raw * inv_a0],
            [1.0, a1_raw * inv_a0, a2_raw * inv_a0],
        )
    }
}

#[allow(dead_code)]
impl AnalogFirstOrder {
    /// Extracts the continuous analog prototype $H(s)$ from 1st-order digital coefficients.
    #[must_use]
    pub fn from_digital(b: [f64; 2], a: [f64; 2], fs_ref: f64) -> Self {
        let k = 2.0 * fs_ref;

        let beta = [f64::midpoint(b[0], b[1]), (0.5 / k) * (b[0] - b[1])];
        let alpha = [f64::midpoint(a[0], a[1]), (0.5 / k) * (a[0] - a[1])];

        Self { beta, alpha }
    }

    /// Discretizes the 1st-order analog prototype into digital coefficients.
    #[must_use]
    pub fn to_digital(self, fs_target: f64) -> ([f64; 2], [f64; 2]) {
        let k = 2.0 * fs_target;

        let b0_raw = self.beta[0] + (self.beta[1] * k);
        let b1_raw = self.beta[0] - (self.beta[1] * k);

        let a0_raw = self.alpha[0] + (self.alpha[1] * k);
        let a1_raw = self.alpha[0] - (self.alpha[1] * k);

        if a0_raw.abs() < 1e-15 {
            return ([1.0, 0.0], [1.0, 0.0]);
        }

        let inv_a0 = 1.0 / a0_raw;
        ([b0_raw * inv_a0, b1_raw * inv_a0], [1.0, a1_raw * inv_a0])
    }
}

/// Directly transforms digital biquad coefficients from `fs_from` to `fs_to`
/// without allocating intermediate structures.
#[must_use]
#[allow(dead_code)]
pub fn transform_biquad(
    b: [f64; 3],
    a: [f64; 3],
    fs_from: f64,
    fs_to: f64,
) -> ([f64; 3], [f64; 3]) {
    if (fs_from - fs_to).abs() < 1e-6 {
        return (b, a);
    }
    let proto = AnalogBiquad::from_digital(b, a, fs_from);
    proto.to_digital(fs_to)
}

/// Transforms a single digital pole $p_z \in (-1, 1)$ across sample rates
/// using continuous BLT prototype recovery.
#[must_use]
#[allow(dead_code)]
pub fn transform_single_pole(pole_z: f64, fs_from: f64, fs_to: f64) -> f64 {
    if (fs_from - fs_to).abs() < 1e-6 {
        return pole_z;
    }
    // Continuous analog pole frequency: p_s = 2 * fs_from * (1 - p_z) / (1 + p_z)
    let denom = 1.0 + pole_z;
    if denom.abs() < 1e-12 {
        return pole_z;
    }
    let p_s = 2.0 * fs_from * (1.0 - pole_z) / denom;
    let k_to = 2.0 * fs_to;
    let new_denom = k_to + p_s;
    if new_denom.abs() < 1e-12 {
        return pole_z;
    }
    (k_to - p_s) / new_denom
}

/// Solves an $N \times N$ linear system $A x = b$ using Gaussian elimination
/// with partial pivoting. Allocates zero heap memory.
#[must_use]
#[allow(dead_code)]
#[allow(clippy::needless_range_loop)]
pub fn solve_linear_system<const N: usize>(
    mut a: [[f64; N]; N],
    mut b: [f64; N],
) -> Option<[f64; N]> {
    for col in 0..N {
        // Find pivot in column
        let mut pivot_row = col;
        let mut max_val = a[col][col].abs();
        for row in (col + 1)..N {
            let val = a[row][col].abs();
            if val > max_val {
                max_val = val;
                pivot_row = row;
            }
        }

        if max_val < 1e-15 {
            return None; // Singular matrix
        }

        // Swap rows if necessary
        if pivot_row != col {
            a.swap(col, pivot_row);
            b.swap(col, pivot_row);
        }

        let pivot = a[col][col];
        for row in (col + 1)..N {
            let factor = a[row][col] / pivot;
            a[row][col] = 0.0;
            for j in (col + 1)..N {
                a[row][j] -= factor * a[col][j];
            }
            b[row] -= factor * b[col];
        }
    }

    // Back-substitution
    let mut x = [0.0; N];
    for row in (0..N).rev() {
        let mut sum = b[row];
        for j in (row + 1)..N {
            sum -= a[row][j] * x[j];
        }
        x[row] = sum / a[row][row];
    }

    Some(x)
}

/// 6x6 Bilinear Transform Matrix for 5th-order polynomials.
/// Satisfies $M \cdot M = 32 \cdot I$.
const BLT_M5: [[f64; 6]; 6] = [
    [1.0, 1.0, 1.0, 1.0, 1.0, 1.0],
    [5.0, 3.0, 1.0, -1.0, -3.0, -5.0],
    [10.0, 2.0, -2.0, -2.0, 2.0, 10.0],
    [10.0, -2.0, -2.0, 2.0, 2.0, -10.0],
    [5.0, -3.0, 1.0, 1.0, -3.0, 5.0],
    [1.0, -1.0, 1.0, -1.0, 1.0, -1.0],
];

fn mat_mul_vec_6(m: &[[f64; 6]; 6], v: &[f64; 6]) -> [f64; 6] {
    let mut out = [0.0; 6];
    for (out_slot, row) in out.iter_mut().zip(m.iter()) {
        *out_slot = row
            .iter()
            .zip(v.iter())
            .fold(0.0, |acc, (&a, &b)| acc + a * b);
    }
    out
}

/// Directly transforms 5th-order IIR filter coefficients from `fs_from` to `fs_to`.
///
/// `a_denom` contains the 5 denominator coefficients $[A_1, A_2, A_3, A_4, A_5]$ where
/// $H(z) = \frac{b_0 + b_1 z^{-1} + \dots + b_5 z^{-5}}{1 - (A_1 z^{-1} + \dots + A_5 z^{-5})}$.
/// Returns $(b_{\text{new}}, a_{\text{denom, new}})$.
#[must_use]
#[allow(clippy::similar_names)]
pub fn transform_order5(
    b: [f64; 6],
    a_denom: [f64; 5],
    fs_from: f64,
    fs_to: f64,
) -> ([f64; 6], [f64; 5]) {
    if (fs_from - fs_to).abs() < 1e-6 {
        return (b, a_denom);
    }
    let ratio = fs_to / fs_from;
    let r2 = ratio * ratio;
    let r3 = r2 * ratio;
    let r4 = r2 * r2;
    let r5 = r4 * ratio;
    let powers = [1.0, ratio, r2, r3, r4, r5];

    // Standard polynomial: a[0] = 1.0, a[k] = -a_denom[k-1]
    let poly_a = [
        1.0,
        -a_denom[0],
        -a_denom[1],
        -a_denom[2],
        -a_denom[3],
        -a_denom[4],
    ];

    // Inverse BLT: c = (1/32) * M * poly
    let c_a_raw = mat_mul_vec_6(&BLT_M5, &poly_a);
    let c_b_raw = mat_mul_vec_6(&BLT_M5, &b);

    let mut c_a_scaled = [0.0; 6];
    let mut c_b_scaled = [0.0; 6];
    for (i, p) in powers.iter().enumerate() {
        c_a_scaled[i] = (c_a_raw[i] * (1.0 / 32.0)) * p;
        c_b_scaled[i] = (c_b_raw[i] * (1.0 / 32.0)) * p;
    }

    // Forward BLT: poly_new = M * c_scaled
    let poly_a_new = mat_mul_vec_6(&BLT_M5, &c_a_scaled);
    let poly_b_new = mat_mul_vec_6(&BLT_M5, &c_b_scaled);

    let a0 = poly_a_new[0];
    if a0.abs() < 1e-15 {
        return (b, a_denom);
    }
    let inv_a0 = 1.0 / a0;

    let b_new = [
        poly_b_new[0] * inv_a0,
        poly_b_new[1] * inv_a0,
        poly_b_new[2] * inv_a0,
        poly_b_new[3] * inv_a0,
        poly_b_new[4] * inv_a0,
        poly_b_new[5] * inv_a0,
    ];
    let a_denom_new = [
        -poly_a_new[1] * inv_a0,
        -poly_a_new[2] * inv_a0,
        -poly_a_new[3] * inv_a0,
        -poly_a_new[4] * inv_a0,
        -poly_a_new[5] * inv_a0,
    ];

    (b_new, a_denom_new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_biquad_identity_roundtrip() {
        // Standard peaking EQ filter at 48 kHz
        // f0 = 1000 Hz, Q = 1.414, gain = +6 dB
        let b = [1.074_864_2, -1.884_205_6, 0.835_341_4];
        let a = [1.0, -1.884_205_6, 0.910_205_6];

        let proto = AnalogBiquad::from_digital(b, a, 48000.0);
        let (b_rec, a_rec) = proto.to_digital(48000.0);

        for i in 0..3 {
            assert!(
                (b[i] - b_rec[i]).abs() < 1e-12,
                "b[{i}] mismatch: {} vs {}",
                b[i],
                b_rec[i]
            );
            assert!(
                (a[i] - a_rec[i]).abs() < 1e-12,
                "a[{i}] mismatch: {} vs {}",
                a[i],
                a_rec[i]
            );
        }
    }

    #[test]
    fn test_biquad_oversample_96k() {
        // 2nd-order Butterworth low-pass at fc = 2000 Hz, fs = 48000 Hz
        let fc = 2000.0;
        let fs1 = 48000.0;
        let omega = 2.0 * std::f64::consts::PI * fc;
        let alpha = [omega * omega, std::f64::consts::SQRT_2 * omega, 1.0];
        let proto = AnalogBiquad {
            beta: [omega * omega, 0.0, 0.0],
            alpha,
        };

        let (b48, a48) = proto.to_digital(fs1);

        // Recover prototype from digital 48k coefficients
        let proto_recovered = AnalogBiquad::from_digital(b48, a48, fs1);

        // Normalize prototypes to compare shape (alpha[2] = 1.0)
        let norm_orig = proto.alpha[2];
        let norm_rec = proto_recovered.alpha[2];

        for i in 0..3 {
            let expected_a = proto.alpha[i] / norm_orig;
            let actual_a = proto_recovered.alpha[i] / norm_rec;
            let rel_err = if expected_a.abs() > 1e-12 {
                ((expected_a - actual_a) / expected_a).abs()
            } else {
                (expected_a - actual_a).abs()
            };
            assert!(
                rel_err < 1e-11,
                "Prototype alpha[{i}] relative mismatch: {rel_err} (expected: {expected_a}, actual: {actual_a})"
            );
        }

        // Re-discretize at 96 kHz
        let (b96, a96) = proto_recovered.to_digital(96000.0);

        // Verify DC gain is identically 1.0 at both 48k and 96k:
        let dc_gain_48 = (b48[0] + b48[1] + b48[2]) / (a48[0] + a48[1] + a48[2]);
        let dc_gain_96 = (b96[0] + b96[1] + b96[2]) / (a96[0] + a96[1] + a96[2]);
        assert!((dc_gain_48 - 1.0).abs() < 1e-12);
        assert!((dc_gain_96 - 1.0).abs() < 1e-12);
    }

    #[test]
    fn test_first_order_identity_roundtrip() {
        let b = [0.15, -0.15];
        let a = [1.0, -0.85];

        let proto = AnalogFirstOrder::from_digital(b, a, 48000.0);
        let (b_rec, a_rec) = proto.to_digital(48000.0);

        for i in 0..2 {
            assert!((b[i] - b_rec[i]).abs() < 1e-12);
            assert!((a[i] - a_rec[i]).abs() < 1e-12);
        }
    }

    #[test]
    fn test_linear_solver_3x3() {
        let a = [[1.0, 2.0, 3.0], [2.0, -1.0, 1.0], [3.0, 0.0, -1.0]];
        let b = [9.0, 8.0, 3.0];

        let sol = solve_linear_system(a, b).expect("System should be solvable");
        // Verify solution: x = [2.0, -1.0, 3.0]
        assert!((sol[0] - 2.0).abs() < 1e-12);
        assert!((sol[1] - (-1.0)).abs() < 1e-12);
        assert!((sol[2] - 3.0).abs() < 1e-12);
    }

    #[test]
    #[allow(clippy::unreadable_literal)]
    fn test_order5_transform_96k() {
        // Tangerine 48k reference coefficients
        let a_48 = [
            4.70560849,
            -8.9041846982,
            8.4675558648,
            -4.0461097341,
            0.77708215683,
        ];
        let b_48 = [
            8.8381632746e-06,
            3.6986391749e-06,
            1.1423493952e-05,
            1.1423493952e-05,
            3.6986391749e-06,
            8.8381632746e-06,
        ];

        let (b_96, a_96) = transform_order5(b_48, a_48, 48000.0, 96000.0);

        // Verify transformed polynomial matches calculated values within numerical tolerance
        assert!((a_96[0] - 4.86279167421).abs() < 1e-6);
        assert!((a_96[1] - (-9.47093718855)).abs() < 1e-6);
        assert!((a_96[2] - 9.23468459081).abs() < 1e-6);
        assert!((a_96[3] - (-4.50779532873)).abs() < 1e-6);
        assert!((a_96[4] - 0.881254644349).abs() < 1e-6);

        // Verify DC gain is preserved
        let dc_48 = (b_48[0] + b_48[1] + b_48[2] + b_48[3] + b_48[4] + b_48[5])
            / (1.0 - (a_48[0] + a_48[1] + a_48[2] + a_48[3] + a_48[4]));
        let dc_96 = (b_96[0] + b_96[1] + b_96[2] + b_96[3] + b_96[4] + b_96[5])
            / (1.0 - (a_96[0] + a_96[1] + a_96[2] + a_96[3] + a_96[4]));

        assert!((dc_48 - dc_96).abs() < 1e-4);
    }
}

//! High-Performance Linear-Phase Polyphase Half-Band Resampler
//!
//! Provides mathematically exact linear-phase 2x and 4x oversampling and host-rate
//! decimation/interpolation, optimized with 64-byte cache alignment for AMD Zen 4
//! and Intel Ivy Bridge (`ThinkPad` T430 generation quad-core i7).

// Disable suboptimal_flops because Intel Ivy Bridge lacks FMA3 hardware instructions.
#![allow(clippy::suboptimal_flops, clippy::excessive_precision)]

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OversamplingMode {
    #[default]
    Off, // 1x: Native processing
    Double, // 2x: e.g. 48 kHz <-> 96 kHz
    Quad,   // 4x: e.g. 48 kHz <-> 192 kHz
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OversamplingPhase {
    Linear, // Flat group delay, linear phase Kaiser half-band (symmetric, constant PDC)
    #[default]
    Minimum, // Zero pre-ringing, minimum phase, ultra-low latency (2/3 samples PDC)
}

impl OversamplingPhase {
    #[must_use]
    pub const fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::Minimum,
            _ => Self::Linear,
        }
    }

    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Linear => 0,
            Self::Minimum => 1,
        }
    }
}

impl OversamplingMode {
    #[must_use]
    pub const fn from_u8(val: u8) -> Self {
        match val {
            1 => Self::Double,
            2 => Self::Quad,
            _ => Self::Off,
        }
    }

    #[must_use]
    pub const fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Double => 1,
            Self::Quad => 2,
        }
    }

    #[must_use]
    pub const fn factor(self) -> usize {
        match self {
            Self::Off => 1,
            Self::Double => 2,
            Self::Quad => 4,
        }
    }

    /// Latency in samples at 1x base sample rate.
    #[must_use]
    pub const fn latency_samples(self, phase: OversamplingPhase) -> u32 {
        match (self, phase) {
            (Self::Off, _) => 0,
            (Self::Double, OversamplingPhase::Linear) => 15,
            (Self::Double, OversamplingPhase::Minimum) => 2,
            (Self::Quad, OversamplingPhase::Linear) => 23,
            (Self::Quad, OversamplingPhase::Minimum) => 3,
        }
    }
}

/// Normalized 31-tap Kaiser half-band filter coefficients (M = 15).
/// Center tap h[15] = 0.5. Even offsets are 0.0.
/// Odd offsets are symmetric: h[15 - (2k+1)] == h[15 + (2k+1)].
const HB31_ODD_COEFFS: [f32; 8] = [
    0.313_137_68,   // c1  (h[14], h[16])
    -0.091_498_785, // c3  (h[12], h[18])
    0.041_893_937,  // c5  (h[10], h[20])
    -0.019_564_148, // c7  (h[8],  h[22])
    0.008_265_147,  // c9  (h[6],  h[24])
    -0.002_869_461, // c11 (h[4],  h[26])
    0.000_692_735,  // c13 (h[2],  h[28])
    -0.000_057_096, // c15 (h[0],  h[30])
];

/// Minimum-phase 31-tap FIR even polyphase branch (pre-scaled by 2.0 for 2x interpolation).
const MIN31_EVEN_COEFFS: [f32; 16] = [
    0.035_268_00,  // 2.0 * h[0]
    0.592_270_45,  // 2.0 * h[2]
    0.634_747_74,  // 2.0 * h[4]
    -0.409_753_15, // 2.0 * h[6]
    0.228_242_18,  // 2.0 * h[8]
    -0.124_905_87, // 2.0 * h[10]
    0.068_018_77,  // 2.0 * h[12]
    -0.036_824_95, // 2.0 * h[14]
    0.019_645_88,  // 2.0 * h[16]
    -0.010_108_35, // 2.0 * h[18]
    0.004_860_59,  // 2.0 * h[20]
    -0.001_965_70, // 2.0 * h[22]
    0.000_566_99,  // 2.0 * h[24]
    -0.000_061_80, // 2.0 * h[26]
    -0.000_001_14, // 2.0 * h[28]
    0.000_000_36,  // 2.0 * h[30]
];

/// Minimum-phase 31-tap FIR odd polyphase branch (pre-scaled by 2.0 for 2x interpolation).
const MIN31_ODD_COEFFS: [f32; 15] = [
    0.217_280_70,  // 2.0 * h[1]
    0.876_457_88,  // 2.0 * h[3]
    -0.033_760_37, // 2.0 * h[5]
    -0.138_021_73, // 2.0 * h[7]
    0.140_568_34,  // 2.0 * h[9]
    -0.104_261_54, // 2.0 * h[11]
    0.065_730_35,  // 2.0 * h[13]
    -0.035_751_25, // 2.0 * h[15]
    0.016_352_88,  // 2.0 * h[17]
    -0.005_843_19, // 2.0 * h[19]
    0.001_381_02,  // 2.0 * h[21]
    -0.000_010_77, // 2.0 * h[23]
    -0.000_156_19, // 2.0 * h[25]
    0.000_036_12,  // 2.0 * h[27]
    -0.000_002_28, // 2.0 * h[29]
];

/// Minimum-phase 31-tap normalized decimation filter coefficients.
const MIN31_COEFFS: [f32; 31] = [
    0.017_634_00,  // h[0]
    0.108_640_35,  // h[1]
    0.296_135_23,  // h[2]
    0.438_228_94,  // h[3]
    0.317_373_87,  // h[4]
    -0.016_880_19, // h[5]
    -0.204_876_58, // h[6]
    -0.069_010_86, // h[7]
    0.114_121_09,  // h[8]
    0.070_284_17,  // h[9]
    -0.062_452_93, // h[10]
    -0.052_130_77, // h[11]
    0.034_009_38,  // h[12]
    0.032_865_17,  // h[13]
    -0.018_412_47, // h[14]
    -0.017_875_62, // h[15]
    0.009_822_94,  // h[16]
    0.008_176_44,  // h[17]
    -0.005_054_17, // h[18]
    -0.002_921_59, // h[19]
    0.002_430_30,  // h[20]
    0.000_690_51,  // h[21]
    -0.000_982_85, // h[22]
    -0.000_005_38, // h[23]
    0.000_283_50,  // h[24]
    -0.000_078_09, // h[25]
    -0.000_030_90, // h[26]
    0.000_018_06,  // h[27]
    -0.000_000_57, // h[28]
    -0.000_001_14, // h[29]
    0.000_000_18,  // h[30]
];

/// Maximum history size for 128-frame chunks (history = 32).
const MAX_STAGE_HISTORY: usize = 32;

/// 64-byte cache-aligned state buffer for a single half-band filter stage.
/// Universal alignment matches both Zen 4 and Ivy Bridge (T430) L1D lines.
#[repr(align(64))]
#[derive(Clone)]
struct HalfBandStage {
    history: [f32; MAX_STAGE_HISTORY],
}

impl Default for HalfBandStage {
    fn default() -> Self {
        Self {
            history: [0.0; MAX_STAGE_HISTORY],
        }
    }
}

impl HalfBandStage {
    fn reset(&mut self) {
        self.history.fill(0.0);
    }

    /// 2x Polyphase Upsampling (Linear or Minimum Phase)
    fn upsample_2x(&mut self, phase: OversamplingPhase, input: &[f32], output: &mut [f32]) {
        match phase {
            OversamplingPhase::Linear => self.upsample_2x_linear(input, output),
            OversamplingPhase::Minimum => self.upsample_2x_minimum(input, output),
        }
    }

    /// 2x Polyphase Downsampling (Linear or Minimum Phase)
    fn downsample_2x(&mut self, phase: OversamplingPhase, input: &[f32], output: &mut [f32]) {
        match phase {
            OversamplingPhase::Linear => self.downsample_2x_linear(input, output),
            OversamplingPhase::Minimum => self.downsample_2x_minimum(input, output),
        }
    }

    /// Linear-Phase 2x Polyphase Upsampling (FIR Interpolation)
    ///
    /// Even output samples y[2n] are direct delayed input copies (multiplication-free).
    /// Odd output samples y[2n + 1] are computed from 8 symmetric odd tap MACs.
    fn upsample_2x_linear(&mut self, input: &[f32], output: &mut [f32]) {
        let n_in = input.len();
        debug_assert_eq!(output.len(), n_in * 2);

        for (i, &x_new) in input.iter().enumerate() {
            // Shift history
            self.history.copy_within(1..16, 0);
            self.history[15] = x_new;

            // Even output phase: direct center tap delay (0.5 * 2.0 = 1.0)
            output[2 * i] = self.history[7];

            // Odd output phase: symmetric folded convolution (2.0 * sum)
            let mut acc = 0.0f32;
            for (k, &c) in HB31_ODD_COEFFS.iter().enumerate() {
                let s_prev = self.history[7 - k];
                let s_next = self.history[8 + k];
                acc += (s_prev + s_next) * c;
            }
            output[2 * i + 1] = acc * 2.0;
        }
    }

    /// Minimum-Phase 2x Polyphase Upsampling (Causal FIR Interpolation)
    ///
    /// Energy is concentrated in earliest taps with zero pre-ringing.
    fn upsample_2x_minimum(&mut self, input: &[f32], output: &mut [f32]) {
        let n_in = input.len();
        debug_assert_eq!(output.len(), n_in * 2);

        for (i, &x_new) in input.iter().enumerate() {
            self.history.copy_within(1..16, 0);
            self.history[15] = x_new;

            // Even output phase: 16 taps
            let mut acc_even = 0.0f32;
            for (k, &c) in MIN31_EVEN_COEFFS.iter().enumerate() {
                acc_even += self.history[15 - k] * c;
            }
            output[2 * i] = acc_even;

            // Odd output phase: 15 taps
            let mut acc_odd = 0.0f32;
            for (k, &c) in MIN31_ODD_COEFFS.iter().enumerate() {
                acc_odd += self.history[15 - k] * c;
            }
            output[2 * i + 1] = acc_odd;
        }
    }

    /// Linear-Phase 2x Polyphase Downsampling (FIR Decimation)
    ///
    /// Low-pass filters at Nyquist/2 and decimates by factor of 2.
    fn downsample_2x_linear(&mut self, input: &[f32], output: &mut [f32]) {
        let n_out = output.len();
        debug_assert_eq!(input.len(), n_out * 2);

        for (i, out_sample) in output.iter_mut().enumerate() {
            let x0 = input[2 * i];
            let x1 = input[2 * i + 1];

            // Advance 2 input samples into history
            self.history.copy_within(2..32, 0);
            self.history[30] = x0;
            self.history[31] = x1;

            // Center tap contribution: 0.5 * x[n - 15]
            let mut acc = 0.5 * self.history[16];

            // Odd tap folded contributions
            for (k, &c) in HB31_ODD_COEFFS.iter().enumerate() {
                let tap_idx = 2 * k + 1;
                let s_prev = self.history[16 - tap_idx];
                let s_next = self.history[16 + tap_idx];
                acc += (s_prev + s_next) * c;
            }
            *out_sample = acc;
        }
    }

    /// Minimum-Phase 2x Polyphase Downsampling (Causal FIR Decimation)
    fn downsample_2x_minimum(&mut self, input: &[f32], output: &mut [f32]) {
        let n_out = output.len();
        debug_assert_eq!(input.len(), n_out * 2);

        for (i, out_sample) in output.iter_mut().enumerate() {
            let x0 = input[2 * i];
            let x1 = input[2 * i + 1];

            self.history.copy_within(2..32, 0);
            self.history[30] = x0;
            self.history[31] = x1;

            let mut acc = 0.0f32;
            for (k, &c) in MIN31_COEFFS.iter().enumerate() {
                acc += self.history[31 - k] * c;
            }
            *out_sample = acc;
        }
    }
}

/// Single-channel cascaded oversampler supporting 1x, 2x, and 4x rates.
#[derive(Clone, Default)]
struct ChannelOversampler {
    up_stage1: HalfBandStage,
    up_stage2: HalfBandStage,
    down_stage1: HalfBandStage,
    down_stage2: HalfBandStage,
    temp_mid: Vec<f32>,
}

impl ChannelOversampler {
    fn reset(&mut self) {
        self.up_stage1.reset();
        self.up_stage2.reset();
        self.down_stage1.reset();
        self.down_stage2.reset();
    }

    fn ensure_capacity(&mut self, mid_frames: usize) {
        if self.temp_mid.len() < mid_frames {
            self.temp_mid.resize(mid_frames, 0.0);
        }
    }

    fn upsample(
        &mut self,
        mode: OversamplingMode,
        phase: OversamplingPhase,
        input: &[f32],
        output: &mut [f32],
    ) {
        let n = input.len();
        match mode {
            OversamplingMode::Off => {
                output[..n].copy_from_slice(input);
            }
            OversamplingMode::Double => {
                self.up_stage1
                    .upsample_2x(phase, input, &mut output[..n * 2]);
            }
            OversamplingMode::Quad => {
                self.ensure_capacity(n * 2);
                self.up_stage1
                    .upsample_2x(phase, input, &mut self.temp_mid[..n * 2]);
                self.up_stage2
                    .upsample_2x(phase, &self.temp_mid[..n * 2], &mut output[..n * 4]);
            }
        }
    }

    fn downsample(
        &mut self,
        mode: OversamplingMode,
        phase: OversamplingPhase,
        input: &[f32],
        output: &mut [f32],
    ) {
        let n_out = output.len();
        match mode {
            OversamplingMode::Off => {
                output.copy_from_slice(&input[..n_out]);
            }
            OversamplingMode::Double => {
                self.down_stage1
                    .downsample_2x(phase, &input[..n_out * 2], output);
            }
            OversamplingMode::Quad => {
                self.ensure_capacity(n_out * 2);
                self.down_stage2.downsample_2x(
                    phase,
                    &input[..n_out * 4],
                    &mut self.temp_mid[..n_out * 2],
                );
                self.down_stage1
                    .downsample_2x(phase, &self.temp_mid[..n_out * 2], output);
            }
        }
    }
}

/// Stereo Linear-Phase or Minimum-Phase Resampler for CLAP Audio Processor
#[derive(Clone, Default)]
pub struct StereoOversampler {
    left: ChannelOversampler,
    right: ChannelOversampler,
    mode: OversamplingMode,
    phase: OversamplingPhase,
}

impl StereoOversampler {
    #[must_use]
    pub fn new(mode: OversamplingMode, phase: OversamplingPhase) -> Self {
        Self {
            left: ChannelOversampler::default(),
            right: ChannelOversampler::default(),
            mode,
            phase,
        }
    }

    pub fn set_config(&mut self, mode: OversamplingMode, phase: OversamplingPhase) {
        if self.mode != mode || self.phase != phase {
            self.mode = mode;
            self.phase = phase;
            self.reset();
        }
    }

    pub fn set_mode(&mut self, mode: OversamplingMode) {
        self.set_config(mode, self.phase);
    }

    pub fn set_phase(&mut self, phase: OversamplingPhase) {
        self.set_config(self.mode, phase);
    }

    #[must_use]
    pub const fn mode(&self) -> OversamplingMode {
        self.mode
    }

    #[must_use]
    pub const fn phase(&self) -> OversamplingPhase {
        self.phase
    }

    pub fn reset(&mut self) {
        self.left.reset();
        self.right.reset();
    }

    pub fn ensure_capacity(&mut self, max_frames: usize) {
        self.left.ensure_capacity(max_frames * 4);
        self.right.ensure_capacity(max_frames * 4);
    }

    /// Upsamples stereo input to the oversampled rate.
    pub fn upsample(&mut self, in_l: &[f32], in_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        let factor = self.mode.factor();
        let n_out = in_l.len() * factor;
        debug_assert!(out_l.len() >= n_out);
        debug_assert!(out_r.len() >= n_out);

        self.left.upsample(self.mode, self.phase, in_l, out_l);
        self.right.upsample(self.mode, self.phase, in_r, out_r);
    }

    /// Downsamples oversampled audio back to base rate.
    pub fn downsample(&mut self, os_l: &[f32], os_r: &[f32], out_l: &mut [f32], out_r: &mut [f32]) {
        self.left.downsample(self.mode, self.phase, os_l, out_l);
        self.right.downsample(self.mode, self.phase, os_r, out_r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_oversampling_modes_and_phases() {
        assert_eq!(OversamplingMode::Off.factor(), 1);
        assert_eq!(OversamplingMode::Double.factor(), 2);
        assert_eq!(OversamplingMode::Quad.factor(), 4);

        assert_eq!(
            OversamplingMode::Off.latency_samples(OversamplingPhase::Linear),
            0
        );
        assert_eq!(
            OversamplingMode::Off.latency_samples(OversamplingPhase::Minimum),
            0
        );

        assert_eq!(
            OversamplingMode::Double.latency_samples(OversamplingPhase::Linear),
            15
        );
        assert_eq!(
            OversamplingMode::Double.latency_samples(OversamplingPhase::Minimum),
            2
        );

        assert_eq!(
            OversamplingMode::Quad.latency_samples(OversamplingPhase::Linear),
            23
        );
        assert_eq!(
            OversamplingMode::Quad.latency_samples(OversamplingPhase::Minimum),
            3
        );
    }

    #[test]
    fn test_linear_phase_reconstruction_2x() {
        let mut oversampler =
            StereoOversampler::new(OversamplingMode::Double, OversamplingPhase::Linear);
        let n = 128;
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        for i in 0..n {
            #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
            let phase = 2.0 * std::f32::consts::PI * 1000.0 * (i as f32) / 48000.0;
            in_l[i] = phase.sin();
            in_r[i] = phase.cos();
        }

        let mut os_l = vec![0.0f32; n * 2];
        let mut os_r = vec![0.0f32; n * 2];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        // Warm up filter history
        for _ in 0..5 {
            oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
            oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);
        }

        oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
        oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);

        // Verify signal magnitude is preserved (0 dB gain in passband)
        let max_in = in_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let max_out = out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(
            (max_out - max_in).abs() < 0.05,
            "Reconstruction gain mismatch: in={max_in}, out={max_out}"
        );
    }

    #[test]
    fn test_linear_phase_reconstruction_4x() {
        let mut oversampler =
            StereoOversampler::new(OversamplingMode::Quad, OversamplingPhase::Linear);
        let n = 128;
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        for i in 0..n {
            #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
            let phase = 2.0 * std::f32::consts::PI * 1000.0 * (i as f32) / 48000.0;
            in_l[i] = phase.sin();
            in_r[i] = phase.cos();
        }

        let mut os_l = vec![0.0f32; n * 4];
        let mut os_r = vec![0.0f32; n * 4];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        // Warm up filter history
        for _ in 0..5 {
            oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
            oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);
        }

        oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
        oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);

        let max_in = in_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let max_out = out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(
            (max_out - max_in).abs() < 0.05,
            "4x reconstruction gain mismatch: in={max_in}, out={max_out}"
        );
    }

    #[test]
    fn test_minimum_phase_reconstruction_2x() {
        let mut oversampler =
            StereoOversampler::new(OversamplingMode::Double, OversamplingPhase::Minimum);
        let n = 128;
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        for i in 0..n {
            #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
            let phase = 2.0 * std::f32::consts::PI * 1000.0 * (i as f32) / 48000.0;
            in_l[i] = phase.sin();
            in_r[i] = phase.cos();
        }

        let mut os_l = vec![0.0f32; n * 2];
        let mut os_r = vec![0.0f32; n * 2];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        // Warm up filter history
        for _ in 0..5 {
            oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
            oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);
        }

        oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
        oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);

        let max_in = in_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let max_out = out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(
            (max_out - max_in).abs() < 0.05,
            "Min-phase 2x gain mismatch: in={max_in}, out={max_out}"
        );
    }

    #[test]
    fn test_minimum_phase_reconstruction_4x() {
        let mut oversampler =
            StereoOversampler::new(OversamplingMode::Quad, OversamplingPhase::Minimum);
        let n = 128;
        let mut in_l = vec![0.0f32; n];
        let mut in_r = vec![0.0f32; n];
        for i in 0..n {
            #[allow(clippy::as_conversions, clippy::cast_precision_loss)]
            let phase = 2.0 * std::f32::consts::PI * 1000.0 * (i as f32) / 48000.0;
            in_l[i] = phase.sin();
            in_r[i] = phase.cos();
        }

        let mut os_l = vec![0.0f32; n * 4];
        let mut os_r = vec![0.0f32; n * 4];
        let mut out_l = vec![0.0f32; n];
        let mut out_r = vec![0.0f32; n];

        // Warm up filter history
        for _ in 0..5 {
            oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
            oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);
        }

        oversampler.upsample(&in_l, &in_r, &mut os_l, &mut os_r);
        oversampler.downsample(&os_l, &os_r, &mut out_l, &mut out_r);

        let max_in = in_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        let max_out = out_l.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
        assert!(
            (max_out - max_in).abs() < 0.05,
            "Min-phase 4x gain mismatch: in={max_in}, out={max_out}"
        );
    }
}

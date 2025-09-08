use core::ops::Range;

use firewheel_core::{
    channel_config::{ChannelConfig, ChannelCount},
    diff::{Diff, Patch},
    dsp::{
        coeff_update::{CoeffUpdateFactor, CoeffUpdateMask},
        declick::{Declicker, FadeType},
        filter::{
            butterworth::Q_BUTTERWORTH_ORD2,
            smoothing_filter::DEFAULT_SMOOTH_SECONDS,
            svf::{SvfCoeff, SvfCoeffSimd, SvfStateSimd},
        },
        volume::{db_to_amp, Volume},
    },
    event::ProcEvents,
    node::{
        AudioNode, AudioNodeInfo, AudioNodeProcessor, ConstructProcessorContext, ProcBuffers,
        ProcExtra, ProcInfo, ProcessStatus,
    },
    param::smoother::{SmoothedParam, SmootherConfig},
    StreamInfo,
};

pub const DEFAULT_Q: f32 = Q_BUTTERWORTH_ORD2;

pub const DEFAULT_MIN_HZ: f32 = 20.0;
pub const DEFAULT_MAX_HZ: f32 = 20_480.0;
pub const DEFAULT_MIN_Q: f32 = 0.02;
pub const DEFAULT_MAX_Q: f32 = 40.0;
pub const DEFAULT_MIN_GAIN_DB: f32 = -24.0;
pub const DEFAULT_MAX_GAIN_DB: f32 = 24.0;

pub type SvfMonoNode = SvfNode<1>;
pub type SvfStereoNode = SvfNode<2>;

#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "bevy", derive(bevy_ecs::prelude::Component))]
#[cfg_attr(feature = "bevy_reflect", derive(bevy_reflect::Reflect))]
pub struct SvfNodeConfig {
    /// The minimum and maximum values for cutoff frequency in hertz.
    ///
    /// By default this is set to `20.0..20480.0`.
    ///
    /// It is generally not recommended to increase this range
    /// unless you know what you are doing.
    pub freq_range: Range<f32>,

    /// The minimum and maximum values for q values.
    ///
    /// By default this is set to `0.02..40.0`.
    ///
    /// It is generally not recommended to increase this range
    /// unless you know what you are doing.
    pub q_range: Range<f32>,

    /// The minimum and maximum values for filter gain (in decibels).
    ///
    /// By default this is set to `-24.0..24.0`.
    ///
    /// It is generally not recommended to increase this range
    /// unless you know what you are doing.
    pub gain_db_range: Range<f32>,
}

impl Default for SvfNodeConfig {
    fn default() -> Self {
        Self {
            freq_range: DEFAULT_MIN_HZ..DEFAULT_MAX_HZ,
            q_range: DEFAULT_MIN_Q..DEFAULT_MAX_Q,
            gain_db_range: DEFAULT_MIN_GAIN_DB..DEFAULT_MAX_GAIN_DB,
        }
    }
}

#[derive(Default, Diff, Patch, Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "bevy", derive(bevy_ecs::prelude::Component))]
#[cfg_attr(feature = "bevy_reflect", derive(bevy_reflect::Reflect))]
pub enum SvfType {
    // Lowpass (-12 dB per octave)
    #[default]
    Lowpass,
    // Lowpass (-24 dB per octave)
    LowpassX2,
    // Lowpass (-12 dB per octave)
    Highpass,
    // Lowpass (-24 dB per octave)
    HighpassX2,
    // Bandpass (-12 dB per octave)
    Bandpass,
    LowShelf,
    HighShelf,
    Bell,
    Notch,
    Allpass,
}

pub fn bandwidth_hz_to_q(bandwidth_hz: f32, cutoff_hz: f32) -> f32 {
    cutoff_hz / bandwidth_hz
}

pub fn bandwidth_octaves_to_q(bandwidth_octaves: f32) -> f32 {
    let two_pow_bw = 2.0f32.powf(bandwidth_octaves);
    two_pow_bw.sqrt() / (two_pow_bw - 1.0)
}

/// An SVF (state variable filter) node.
///
/// This is based on the filter model developed by Andrew Simper:
/// <https://cytomic.com/files/dsp/SvfLinearTrapOptimised2.pdf>
#[cfg_attr(feature = "bevy", derive(bevy_ecs::prelude::Component))]
#[cfg_attr(feature = "bevy_reflect", derive(bevy_reflect::Reflect))]
#[derive(Diff, Patch, Debug, Clone, Copy, PartialEq)]
pub struct SvfNode<const CHANNELS: usize> {
    /// The type of filter
    pub filter_type: SvfType,

    /// The cutoff frequency in hertz in the range `[20.0, 20480.0]`.
    pub cutoff_hz: f32,
    /// The quality (q) factor
    ///
    /// This is also sometimes referred to as "bandwidth", but note the
    /// formula to convert bandwidth in hertz to q is:
    ///
    /// `Q = cutoff_hz / BW`
    ///
    /// and the formula to convert bandwidth in octaves to q is:
    ///
    /// `Q = sqrt(2^BW) / (2^BW - 1)`
    pub q_factor: f32,
    /// The filter gain
    ///
    /// This only has effect if the filter type is one of the following:
    /// * [`SvfType::LowShelf`]
    /// * [`SvfType::HighShelf`]
    /// * [`SvfType::Bell`]
    pub gain: Volume,
    /// Whether or not this node is enabled.
    pub enabled: bool,

    /// The time in seconds of the internal smoothing filter.
    ///
    /// By default this is set to `0.015` (15ms).
    pub smooth_seconds: f32,

    /// An exponent representing the rate at which DSP coefficients are
    /// updated when parameters are being smoothed.
    ///
    /// Smaller values will produce less "stair-stepping" artifacts,
    /// but will also consume more CPU.
    ///
    /// The resulting number of frames (samples in a single channel of audio)
    /// that will elapse between each update is calculated as
    /// `2^coeff_update_factor`.
    ///
    /// By default this is set to `5`.
    pub coeff_update_factor: CoeffUpdateFactor,
}

impl<const CHANNELS: usize> Default for SvfNode<CHANNELS> {
    fn default() -> Self {
        Self {
            filter_type: SvfType::Lowpass,
            cutoff_hz: 1_000.0,
            q_factor: DEFAULT_Q,
            gain: Volume::Decibels(0.0),
            enabled: true,
            smooth_seconds: DEFAULT_SMOOTH_SECONDS,
            coeff_update_factor: CoeffUpdateFactor(5),
        }
    }
}

impl<const CHANNELS: usize, E> AudioNode<E> for SvfNode<CHANNELS> {
    type Configuration = SvfNodeConfig;

    fn info(&self, _config: &Self::Configuration) -> AudioNodeInfo {
        AudioNodeInfo::new()
            .debug_name("svf")
            .channel_config(ChannelConfig {
                num_inputs: ChannelCount::new(CHANNELS as u32).unwrap(),
                num_outputs: ChannelCount::new(CHANNELS as u32).unwrap(),
            })
    }

    fn construct_processor(
        &self,
        config: &Self::Configuration,
        cx: ConstructProcessorContext,
    ) -> impl AudioNodeProcessor<E> {
        let cutoff_hz = self
            .cutoff_hz
            .clamp(config.freq_range.start, config.freq_range.end);
        let q_factor = self
            .q_factor
            .clamp(config.q_range.start, config.q_range.end);

        let min_gain = db_to_amp(config.gain_db_range.start);
        let max_gain = db_to_amp(config.gain_db_range.end);
        let mut gain = self.gain.amp().clamp(min_gain, max_gain);
        if gain > 0.99999 && gain < 1.00001 {
            gain = 1.0;
        }

        let mut new_self = Processor {
            filter_0: SvfStateSimd::<CHANNELS>::default(),
            filter_1: SvfStateSimd::<CHANNELS>::default(),
            num_filters: 0,
            filter_0_coeff: SvfCoeffSimd::<CHANNELS>::default(),
            filter_1_coeff: SvfCoeffSimd::<CHANNELS>::default(),
            filter_type: self.filter_type,
            cutoff_hz: SmoothedParam::new(
                cutoff_hz,
                SmootherConfig {
                    smooth_seconds: self.smooth_seconds,
                    ..Default::default()
                },
                cx.stream_info.sample_rate,
            ),
            q_factor: SmoothedParam::new(
                q_factor,
                SmootherConfig {
                    smooth_seconds: self.smooth_seconds,
                    ..Default::default()
                },
                cx.stream_info.sample_rate,
            ),
            gain: SmoothedParam::new(
                gain,
                SmootherConfig {
                    smooth_seconds: self.smooth_seconds,
                    ..Default::default()
                },
                cx.stream_info.sample_rate,
            ),
            enable_declicker: Declicker::from_enabled(self.enabled),
            freq_range: config.freq_range.clone(),
            q_range: config.q_range.clone(),
            gain_range: min_gain..max_gain,
            coeff_update_mask: self.coeff_update_factor.mask(),
        };

        new_self.calc_coefficients(cx.stream_info.sample_rate_recip as f32);

        new_self
    }
}

struct Processor<const CHANNELS: usize> {
    filter_0: SvfStateSimd<CHANNELS>,
    filter_1: SvfStateSimd<CHANNELS>,
    num_filters: usize,

    filter_0_coeff: SvfCoeffSimd<CHANNELS>,
    filter_1_coeff: SvfCoeffSimd<CHANNELS>,

    filter_type: SvfType,
    cutoff_hz: SmoothedParam,
    q_factor: SmoothedParam,
    gain: SmoothedParam,

    enable_declicker: Declicker,

    freq_range: Range<f32>,
    q_range: Range<f32>,
    gain_range: Range<f32>,
    coeff_update_mask: CoeffUpdateMask,
}

impl<const CHANNELS: usize> Processor<CHANNELS> {
    pub fn calc_coefficients(&mut self, sample_rate_recip: f32) {
        let cutoff_hz = self.cutoff_hz.target_value();
        let q = self.q_factor.target_value();
        let gain = self.gain.target_value();

        match self.filter_type {
            SvfType::Lowpass => {
                self.num_filters = 1;

                self.filter_0_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::lowpass_ord2(cutoff_hz, q, sample_rate_recip));
            }
            SvfType::LowpassX2 => {
                self.num_filters = 2;

                let [coeff_0, coeff_1] = SvfCoeff::lowpass_ord4(cutoff_hz, q, sample_rate_recip);
                self.filter_0_coeff = SvfCoeffSimd::splat(coeff_0);
                self.filter_1_coeff = SvfCoeffSimd::splat(coeff_1);
            }
            SvfType::Highpass => {
                self.num_filters = 1;

                self.filter_0_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::highpass_ord2(cutoff_hz, q, sample_rate_recip));
            }
            SvfType::HighpassX2 => {
                self.num_filters = 2;

                let [coeff_0, coeff_1] = SvfCoeff::highpass_ord4(cutoff_hz, q, sample_rate_recip);
                self.filter_0_coeff = SvfCoeffSimd::splat(coeff_0);
                self.filter_1_coeff = SvfCoeffSimd::splat(coeff_1);
            }
            SvfType::Bandpass => {
                self.num_filters = 2;

                self.filter_0_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::lowpass_ord2(cutoff_hz, q, sample_rate_recip));
                self.filter_1_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::highpass_ord2(cutoff_hz, q, sample_rate_recip));
            }
            SvfType::LowShelf => {
                self.num_filters = 1;

                self.filter_0_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::low_shelf(cutoff_hz, q, gain, sample_rate_recip));
            }
            SvfType::HighShelf => {
                self.num_filters = 1;

                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::high_shelf(
                    cutoff_hz,
                    q,
                    gain,
                    sample_rate_recip,
                ));
            }
            SvfType::Bell => {
                self.num_filters = 1;

                self.filter_0_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::bell(cutoff_hz, q, gain, sample_rate_recip));
            }
            SvfType::Notch => {
                self.num_filters = 1;

                self.filter_0_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::notch(cutoff_hz, q, sample_rate_recip));
            }
            SvfType::Allpass => {
                self.num_filters = 1;

                self.filter_0_coeff =
                    SvfCoeffSimd::splat(SvfCoeff::allpass(cutoff_hz, q, sample_rate_recip));
            }
        }

        if self.num_filters == 1 {
            self.filter_1.reset();
        }
    }

    fn loop_lowpass_ord2(
        &mut self,
        info: &ProcInfo,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::lowpass_ord2(
                    cutoff_hz,
                    q,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let out = self.filter_0.process(s, &self.filter_0_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_lowpass_ord4(
        &mut self,
        info: &ProcInfo,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                let [coeff_0, coeff_1] =
                    SvfCoeff::lowpass_ord4(cutoff_hz, q, info.sample_rate_recip as f32);
                self.filter_0_coeff = SvfCoeffSimd::splat(coeff_0);
                self.filter_1_coeff = SvfCoeffSimd::splat(coeff_1);
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let s = self.filter_0.process(s, &self.filter_0_coeff);
            let out = self.filter_1.process(s, &self.filter_1_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_highpass_ord2(
        &mut self,
        info: &ProcInfo,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::highpass_ord2(
                    cutoff_hz,
                    q,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let out = self.filter_0.process(s, &self.filter_0_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_highpass_ord4(
        &mut self,
        info: &ProcInfo,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
    ) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                let [coeff_0, coeff_1] =
                    SvfCoeff::highpass_ord4(cutoff_hz, q, info.sample_rate_recip as f32);
                self.filter_0_coeff = SvfCoeffSimd::splat(coeff_0);
                self.filter_1_coeff = SvfCoeffSimd::splat(coeff_1);
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let s = self.filter_0.process(s, &self.filter_0_coeff);
            let out = self.filter_1.process(s, &self.filter_1_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_bandpass(&mut self, info: &ProcInfo, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::lowpass_ord2(
                    cutoff_hz,
                    q,
                    info.sample_rate_recip as f32,
                ));
                self.filter_1_coeff = SvfCoeffSimd::splat(SvfCoeff::highpass_ord2(
                    cutoff_hz,
                    q,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let s = self.filter_0.process(s, &self.filter_0_coeff);
            let out = self.filter_1.process(s, &self.filter_1_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_low_shelf(&mut self, info: &ProcInfo, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();
            let gain = self.gain.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::low_shelf(
                    cutoff_hz,
                    q,
                    gain,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let out = self.filter_0.process(s, &self.filter_0_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_high_shelf(&mut self, info: &ProcInfo, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();
            let gain = self.gain.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::high_shelf(
                    cutoff_hz,
                    q,
                    gain,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let out = self.filter_0.process(s, &self.filter_0_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_bell(&mut self, info: &ProcInfo, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();
            let gain = self.gain.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::bell(
                    cutoff_hz,
                    q,
                    gain,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let out = self.filter_0.process(s, &self.filter_0_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_notch(&mut self, info: &ProcInfo, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::notch(
                    cutoff_hz,
                    q,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let out = self.filter_0.process(s, &self.filter_0_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }

    fn loop_allpass(&mut self, info: &ProcInfo, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        assert!(inputs.len() == CHANNELS);
        assert!(outputs.len() == CHANNELS);
        for ch in inputs.iter() {
            assert!(ch.len() >= info.frames);
        }
        for ch in outputs.iter() {
            assert!(ch.len() >= info.frames);
        }

        for i in 0..info.frames {
            let cutoff_hz = self.cutoff_hz.next_smoothed();
            let q = self.q_factor.next_smoothed();

            // Because recalculating filter coefficients is expensive, a trick like
            // this can be used to only recalculate them every few frames.
            //
            // TODO: use core::hint::cold_path() once that stabilizes
            //
            // TODO: Alternatively, this could be optimized using a lookup table
            if self.coeff_update_mask.do_update(i) {
                self.filter_0_coeff = SvfCoeffSimd::splat(SvfCoeff::allpass(
                    cutoff_hz,
                    q,
                    info.sample_rate_recip as f32,
                ));
            }

            let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                // Safety: These bounds have been checked above.
                unsafe { *inputs.get_unchecked(ch_i).get_unchecked(i) }
            });

            let out = self.filter_0.process(s, &self.filter_0_coeff);

            for ch_i in 0..CHANNELS {
                // Safety: These bounds have been checked above.
                unsafe {
                    *outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) = out[ch_i];
                }
            }
        }
    }
}

impl<const CHANNELS: usize, E> AudioNodeProcessor<E> for Processor<CHANNELS> {
    fn process(
        &mut self,
        info: &ProcInfo,
        buffers: ProcBuffers,
        events: &mut ProcEvents<E>,
        extra: &mut ProcExtra,
    ) -> ProcessStatus {
        let mut params_changed = false;

        for patch in events.drain_patches::<SvfNode<CHANNELS>>() {
            match patch {
                SvfNodePatch::FilterType(filter_type) => {
                    params_changed = true;
                    self.filter_type = filter_type;
                }
                SvfNodePatch::CutoffHz(cutoff) => {
                    params_changed = true;
                    self.cutoff_hz
                        .set_value(cutoff.clamp(self.freq_range.start, self.freq_range.end));
                }
                SvfNodePatch::QFactor(q_factor) => {
                    params_changed = true;
                    self.q_factor
                        .set_value(q_factor.clamp(self.q_range.start, self.q_range.end));
                }
                SvfNodePatch::Gain(gain) => {
                    params_changed = true;
                    let mut gain = gain.amp().clamp(self.gain_range.start, self.gain_range.end);
                    if gain > 0.99999 && gain < 1.00001 {
                        gain = 1.0;
                    }
                    self.gain.set_value(gain);
                }
                SvfNodePatch::Enabled(enabled) => {
                    // Tell the declicker to crossfade.
                    self.enable_declicker
                        .fade_to_enabled(enabled, &extra.declick_values);
                }
                SvfNodePatch::SmoothSeconds(seconds) => {
                    self.cutoff_hz.set_smooth_seconds(seconds, info.sample_rate);
                }
                SvfNodePatch::CoeffUpdateFactor(f) => {
                    self.coeff_update_mask = f.mask();
                }
            }
        }

        if self.enable_declicker.disabled() {
            // Disabled. Bypass this node.
            return ProcessStatus::Bypass;
        }

        if info.in_silence_mask.all_channels_silent(CHANNELS) && self.enable_declicker.is_settled()
        {
            // Outputs will be silent, so no need to process.

            // Reset the smoothers and filters since they don't need to smooth any
            // output.
            self.cutoff_hz.reset();
            self.filter_0.reset();
            self.filter_1.reset();
            self.enable_declicker.reset_to_target();

            return ProcessStatus::ClearAllOutputs;
        }

        if self.cutoff_hz.is_smoothing() || self.q_factor.is_smoothing() || self.gain.is_smoothing()
        {
            match self.filter_type {
                SvfType::Lowpass => self.loop_lowpass_ord2(info, buffers.inputs, buffers.outputs),
                SvfType::LowpassX2 => self.loop_lowpass_ord4(info, buffers.inputs, buffers.outputs),
                SvfType::Highpass => self.loop_highpass_ord2(info, buffers.inputs, buffers.outputs),
                SvfType::HighpassX2 => {
                    self.loop_highpass_ord4(info, buffers.inputs, buffers.outputs)
                }
                SvfType::Bandpass => self.loop_bandpass(info, buffers.inputs, buffers.outputs),
                SvfType::LowShelf => self.loop_low_shelf(info, buffers.inputs, buffers.outputs),
                SvfType::HighShelf => self.loop_high_shelf(info, buffers.inputs, buffers.outputs),
                SvfType::Bell => self.loop_bell(info, buffers.inputs, buffers.outputs),
                SvfType::Notch => self.loop_notch(info, buffers.inputs, buffers.outputs),
                SvfType::Allpass => self.loop_allpass(info, buffers.inputs, buffers.outputs),
            }

            if self.cutoff_hz.settle() && self.q_factor.settle() && self.gain.settle() {
                self.calc_coefficients(info.sample_rate_recip as f32);
            }
        } else {
            // The cutoff parameter is not currently smoothing, so we can optimize by
            // only updating the filter coefficients once.
            if params_changed {
                self.calc_coefficients(info.sample_rate_recip as f32);
            }

            assert!(buffers.inputs.len() == CHANNELS);
            assert!(buffers.outputs.len() == CHANNELS);
            for ch in buffers.inputs.iter() {
                assert!(ch.len() >= info.frames);
            }
            for ch in buffers.outputs.iter() {
                assert!(ch.len() >= info.frames);
            }

            if self.num_filters == 1 {
                for i in 0..info.frames {
                    let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                        // Safety: These bounds have been checked above.
                        unsafe { *buffers.inputs.get_unchecked(ch_i).get_unchecked(i) }
                    });

                    let out = self.filter_0.process(s, &self.filter_0_coeff);

                    for ch_i in 0..CHANNELS {
                        // Safety: These bounds have been checked above.
                        unsafe {
                            *buffers.outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) =
                                out[ch_i];
                        }
                    }
                }
            } else {
                for i in 0..info.frames {
                    let s: [f32; CHANNELS] = core::array::from_fn(|ch_i| {
                        // Safety: These bounds have been checked above.
                        unsafe { *buffers.inputs.get_unchecked(ch_i).get_unchecked(i) }
                    });

                    let s = self.filter_0.process(s, &self.filter_0_coeff);
                    let out = self.filter_1.process(s, &self.filter_1_coeff);

                    for ch_i in 0..CHANNELS {
                        // Safety: These bounds have been checked above.
                        unsafe {
                            *buffers.outputs.get_unchecked_mut(ch_i).get_unchecked_mut(i) =
                                out[ch_i];
                        }
                    }
                }
            }
        }

        // Crossfade between the wet and dry signals to declick enabling/disabling.
        self.enable_declicker.process_crossfade(
            buffers.inputs,
            buffers.outputs,
            info.frames,
            &extra.declick_values,
            FadeType::Linear,
        );

        ProcessStatus::outputs_not_silent()
    }

    fn new_stream(&mut self, stream_info: &StreamInfo) {
        self.cutoff_hz.update_sample_rate(stream_info.sample_rate);
        self.q_factor.update_sample_rate(stream_info.sample_rate);
        self.gain.update_sample_rate(stream_info.sample_rate);

        self.calc_coefficients(stream_info.sample_rate_recip as f32);
    }
}

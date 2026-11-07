//! Combining a group's multiplexes into one stream (see `docs/design/multi-multiplex.md`).
//!
//! Every multiplex produces 48 kHz complex baseband in its own [`Station`](crate::Station).
//! The combiner shifts each one to its place around the group's centre, sums them and
//! brings the sum to the radio's sample rate:
//!
//! - a span that fits an intermediate rate (48, 96 or 192 kHz): mix at that rate and
//!   resample **once** to the radio's rate (the expensive stage is not multiplied);
//! - a wider span (up to 1 MHz): resample every multiplex to the radio's rate on its
//!   own, then mix and sum.
//!
//! The group's IF - where the centre sits relative to the local oscillator - is not
//! applied here: the HackRF sink's mixer shifts the whole stream, as for one multiplex.

use crate::config::ResamplerSetting;
use crate::error::{Result, StationError};
use crate::plan::GroupPlan;
use decdrm_io::Resampler;

const TAU: f64 = std::f64::consts::TAU;

/// Renormalize every 4096 samples (the rotation's rounding would otherwise drift).
const RENORMALIZE_MASK: usize = 4095;

/// (cos, sin) of an angle, in the order the loops use them.
fn sin_cos(angle: f64) -> (f64, f64) {
    let (sin, cos) = angle.sin_cos();
    (cos, sin)
}

/// Several multiplexes' 48 kHz baseband on their way to one stream at the radio's rate.
pub(crate) struct Combiner {
    /// Per multiplex: 48 kHz -> `rate` (absent when `rate` is 48 kHz).
    stages: Vec<Option<Resampler>>,
    /// Per multiplex: the mixer's current rotation (cos, sin) and the step's, both on
    /// the unit circle; one complex multiply per sample, no `sin_cos` in the loops.
    phasors: Vec<(f64, f64)>,
    steps: Vec<(f64, f64)>,
    /// The composite at `rate`, interleaved I/Q.
    mix: Vec<f32>,
    /// One multiplex's resampled frame, reused.
    scratch: Vec<f32>,
    /// `rate` -> the radio's rate (absent with the per-multiplex strategy).
    shared: Option<Resampler>,
}

impl Combiner {
    /// A combiner for `group`; `quality` is the resampler quality of `[output.hackrf]`.
    pub fn new(group: &GroupPlan, quality: ResamplerSetting) -> Result<Self> {
        let rate = group.intermediate_hz.unwrap_or(group.sample_rate_hz);
        let mut stages = Vec::with_capacity(group.multiplexes.len());
        let mut phasors = Vec::with_capacity(group.multiplexes.len());
        let mut steps = Vec::with_capacity(group.multiplexes.len());
        for m in &group.multiplexes {
            stages.push(if rate == 48_000 {
                None
            } else {
                Some(Resampler::new(48_000, rate, 2, quality.quality()).map_err(StationError::Output)?)
            });
            // Where the multiplex sits relative to the group centre, before the group's
            // IF moves the whole stream.
            let offset_hz = m.offset_hz - group.if_offset_hz;
            phasors.push((1.0, 0.0));
            steps.push(sin_cos(TAU * offset_hz / f64::from(rate)));
        }
        let shared = if rate == group.sample_rate_hz {
            None
        } else {
            Some(Resampler::new(rate, group.sample_rate_hz, 2, quality.quality()).map_err(StationError::Output)?)
        };
        Ok(Self { stages, phasors, steps, mix: Vec::new(), scratch: Vec::new(), shared })
    }


    /// Start a frame: the following [`Self::push`] calls sum into an empty composite.
    pub fn begin_frame(&mut self) {
        self.mix.clear();
    }

    /// Add multiplex `index`'s 48 kHz interleaved I/Q frame to the composite.
    pub fn push(&mut self, index: usize, samples: &[f32]) {
        let Self { stages, scratch, mix, phasors, steps, .. } = self;
        let stage = &mut stages[index];
        scratch.clear();
        match stage {
            Some(r) => r.process_into(samples, scratch),
            None => scratch.extend_from_slice(samples),
        }
        if mix.len() < scratch.len() {
            mix.resize(scratch.len(), 0.0);
        }
        let (mut cos, mut sin) = phasors[index];
        let (step_cos, step_sin) = steps[index];
        for (n, &[i, q]) in scratch.as_chunks::<2>().0.iter().enumerate() {
            let (i, q) = (f64::from(i), f64::from(q));
            mix[2 * n] += (i * cos - q * sin) as f32;
            mix[2 * n + 1] += (i * sin + q * cos) as f32;
            let (nc, ns) = (cos * step_cos - sin * step_sin, cos * step_sin + sin * step_cos);
            (cos, sin) = (nc, ns);
            if n & RENORMALIZE_MASK == RENORMALIZE_MASK {
                let m = (cos * cos + sin * sin).sqrt();
                (cos, sin) = (cos / m, sin / m);
            }
        }
        phasors[index] = (cos, sin);
    }

    /// Finish the frame: the composite at the radio's rate, interleaved I/Q.
    pub fn finish_frame(&mut self, out: &mut Vec<f32>) {
        out.clear();
        match self.shared.as_mut() {
            Some(r) => r.process_into(&self.mix, out),
            None => out.extend_from_slice(&self.mix),
        }
    }

    /// The tail of the last frames, for [`Station::finish`](crate::Station::finish):
    /// the per-multiplex stages' tails, through the shared resampler if there is one.
    pub fn flush(&mut self, out: &mut Vec<f32>) {
        out.clear();
        self.mix_tail();
        let Some(shared) = self.shared.as_mut() else {
            out.extend_from_slice(&self.mix);
            return;
        };
        let mix = std::mem::take(&mut self.mix);
        if !mix.is_empty() {
            shared.process_into(&mix, out);
        }
        self.mix = mix;
        out.extend(shared.flush());
    }

    /// Every per-multiplex stage's tail, shifted and summed into [`Self::mix`].
    fn mix_tail(&mut self) {
        let Self { stages, scratch, mix, phasors, steps, .. } = self;
        mix.clear();
        for (i, stage) in stages.iter_mut().enumerate() {
            let Some(r) = stage else { continue };
            scratch.clear();
            scratch.extend(r.flush());
            if mix.len() < scratch.len() {
                mix.resize(scratch.len(), 0.0);
            }
            let (step_cos, step_sin) = steps[i];
            let (mut cos, mut sin) = phasors[i];
            for (n, &[re, im]) in scratch.as_chunks::<2>().0.iter().enumerate() {
                let (iv, qv) = (f64::from(re), f64::from(im));
                mix[2 * n] += (iv * cos - qv * sin) as f32;
                mix[2 * n + 1] += (iv * sin + qv * cos) as f32;
                let (nc, ns) = (cos * step_cos - sin * step_sin, cos * step_sin + sin * step_cos);
                (cos, sin) = (nc, ns);
                if n & RENORMALIZE_MASK == RENORMALIZE_MASK {
                    let m = (cos * cos + sin * sin).sqrt();
                    (cos, sin) = (cos / m, sin / m);
                }
            }
            phasors[i] = (cos, sin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StationConfig;

    const TWO: &str = r#"
[output]
format = "iq"
[output.hackrf]
sample_rate_hz = 2_000_000
if_offset_hz = -250_000
resampler = "fast"
[[multiplex]]
frequency_hz = 25_760_000
mode = "B"
occupancy = 3
level_dbfs = -18
[[multiplex.service]]
label = "A"
id = 0x42
[multiplex.service.audio]
codec = "he-aac"
input = { tone_hz = 440.0 }
[[multiplex]]
frequency_hz = 25_780_000
mode = "B"
occupancy = 3
level_dbfs = -18
[[multiplex.service]]
label = "B"
id = 0x43
[multiplex.service.audio]
codec = "he-aac"
input = { tone_hz = 880.0 }
"#;

    /// Two multiplexes 20 kHz apart: each plain carrier lands at its offset around the
    /// group's centre, and the group's IF is not applied here.
    #[test]
    fn two_multiplexes_land_on_their_offsets() {
        let cfg = StationConfig::from_toml_str(TWO).unwrap();
        let crate::plan::StationPlan::Group(group) = cfg.validate_station().unwrap() else {
            panic!("expected a group")
        };
        let mut combiner = Combiner::new(&group, crate::config::ResamplerSetting::Fast).unwrap();
        let mut out = Vec::new();
        for _ in 0..2 {
            combiner.begin_frame();
            for i in 0..group.multiplexes.len() {
                let mut frame = vec![0.0f32; 19_200 * 2];
                for z in frame.as_chunks_mut::<2>().0 {
                    z[0] = 1.0;
                }
                combiner.push(i, &frame);
            }
            combiner.finish_frame(&mut out);
        }
        let rate = f64::from(group.sample_rate_hz);
        let magnitude = |hz: f64| {
            let step = TAU * hz / rate;
            let (mut re, mut im, mut phase) = (0.0f64, 0.0f64, 0.0f64);
            for &[i, q] in out.as_chunks::<2>().0 {
                let (sin, cos) = phase.sin_cos();
                re += f64::from(i) * cos + f64::from(q) * sin;
                im += f64::from(q) * cos - f64::from(i) * sin;
                phase += step;
            }
            (re * re + im * im).sqrt() / (out.len() as f64 / 2.0)
        };
        for m in &group.multiplexes {
            let offset = m.offset_hz - group.if_offset_hz;
            let peak = magnitude(offset);
            assert!(
                peak > 10.0 * magnitude(offset + 5_000.0),
                "{offset} Hz: {peak} against {}",
                magnitude(offset + 5_000.0)
            );
        }
        assert!(magnitude(0.0) < magnitude(10_000.0) / 10.0, "nothing at the centre");
    }
}
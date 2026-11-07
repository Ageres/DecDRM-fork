//! A station that sends several multiplexes through one HackRF One (`[[multiplex]]`).
//!
//! Every multiplex runs as its own [`Station`] (its encoders, its plan, its channel
//! simulator) but with no output of its own: each frame's 48 kHz complex baseband goes
//! through the [`Combiner`], which places the multiplexes around the group's centre and
//! brings the sum to the radio's rate. One [`HackrfSink`] sends the result.
//!
//! The multiplexes are built from the group plan ([`crate::plan::StationPlan::Group`]);
//! the radio's settings (sample rate, gain, the amplifier, the group's IF) come from
//! `[output.hackrf]`, and its nominal frequency is the group's centre.

use crate::combine::Combiner;
use crate::config::StationConfig;
use crate::error::{Result, StationError};
use crate::hackrf::HackrfSink;
use crate::plan::{FRAME_SECONDS, GroupPlan};
use crate::station::{Station, StationStatus, StopHandle};

/// A group of multiplexes transmitting through one HackRF One.
pub struct GroupStation {
    group: GroupPlan,
    /// One per multiplex, each feeding no sink.
    stations: Vec<Station>,
    combiner: Combiner,
    sink: HackrfSink,
    /// The combined stream of the last frame (the radio's rate, interleaved I/Q).
    samples: Vec<f32>,
    stop: StopHandle,
    frames: u64,
    status: StationStatus,
}

impl GroupStation {
    /// Open the radio, create every multiplex's station and the combiner.
    pub fn new(cfg: StationConfig, group: GroupPlan) -> Result<Self> {
        Self::with_stop(cfg, group, StopHandle::default())
    }

    /// Like [`Self::new`] with a stop handle made beforehand (see
    /// [`Station::with_plan_and_stop`]).
    pub fn with_stop(cfg: StationConfig, group: GroupPlan, stop: StopHandle) -> Result<Self> {
        let Some(h) = cfg.output.hackrf.clone() else {
            return Err(StationError::config("a group needs [output.hackrf] (one stream, one radio)"));
        };
        // The radio sends the group's centre; its IF is the group's.
        let centre_hz = (group.lo_hz + group.if_offset_hz).round();
        if !(1_000_000.0..=6_000_000_000.0).contains(&centre_hz) {
            return Err(StationError::config("the group's centre is outside the HackRF's 1 MHz-6 GHz range"));
        }
        let mut settings = h.clone();
        settings.frequency_hz = centre_hz as u64;
        settings.if_offset_hz = group.if_offset_hz;
        let sink = HackrfSink::open_resampled(&settings)?;
        let combiner = Combiner::new(&group, h.resampler)?;
        // Every station waits for its first web-stream audio (up to 20 s), so they
        // start together: three streams must not add up to a minute.
        let started = std::thread::scope(|scope| {
            let handles: Vec<_> = group
                .multiplexes
                .iter()
                .map(|m| {
                    let mut sub = m.cfg.clone();
                    // The multiplex's samples go to the combiner, not to a sink.
                    sub.output.file = None;
                    sub.output.device = None;
                    sub.output.hackrf = None;
                    let plan = m.plan.clone();
                    let stop = stop.clone();
                    scope.spawn(move || Station::with_plan_and_stop(sub, plan, stop))
                })
                .collect();
            handles.into_iter().map(|h| h.join()).collect::<Vec<_>>()
        });
        let mut stations = Vec::with_capacity(started.len());
        for started in started {
            stations.push(started.unwrap_or_else(|_| {
                Err(StationError::config("a multiplex's station panicked while starting"))
            })?);
        }
        let mut s = Self {
            group,
            stations,
            combiner,
            sink,
            samples: Vec::new(),
            stop,
            frames: 0,
            status: StationStatus::default(),
        };
        s.update_status();
        Ok(s)
    }

    /// Produce the next 400 ms: every multiplex's frame, combined and sent to the
    /// radio. Waits while the radio's queue is full (pacing the group to real time).
    pub fn transmit_frame(&mut self) -> Result<&[f32]> {
        self.combiner.begin_frame();
        let mut frames = Vec::with_capacity(self.stations.len());
        for station in &mut self.stations {
            frames.push(station.transmit_frame()?);
        }
        self.combiner.push_frame(&frames);
        self.combiner.finish_frame(&mut self.samples);
        self.sink.write(&self.samples, &self.stop)?;
        self.frames += 1;
        self.update_status();
        Ok(&self.samples)
    }

    /// Produce `n` frames.
    pub fn run_frames(&mut self, n: u64) -> Result<()> {
        for _ in 0..n {
            self.transmit_frame()?;
        }
        Ok(())
    }

    /// End the transmission: the combiner's tail, then the radio's drain (unless
    /// stopped). Returns the final status.
    pub fn finish(mut self) -> Result<StationStatus> {
        self.combiner.flush(&mut self.samples);
        if !self.samples.is_empty() {
            self.sink.write(&self.samples, &self.stop)?;
        }
        self.sink.drain(&self.stop)?;
        self.update_status();
        Ok(self.status)
    }

    /// The group plan.
    pub fn plan(&self) -> &GroupPlan {
        &self.group
    }

    /// The current status (the first multiplex's services; the stream's output and the
    /// radio's counters are the group's).
    pub fn status(&self) -> &StationStatus {
        &self.status
    }

    /// A handle that stops the station's waits from another thread.
    pub fn stop_handle(&self) -> StopHandle {
        self.stop.clone()
    }

    /// Load every multiplex's Journaline page files again now; returns the number of
    /// Journaline applications.
    pub fn reload_journaline(&mut self) -> usize {
        self.stations.iter_mut().map(Station::reload_journaline).sum()
    }

    /// Log lines since the last call, each prefixed with its multiplex's frequency.
    pub fn take_log(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for (m, station) in self.group.multiplexes.iter().zip(self.stations.iter_mut()) {
            lines.extend(
                station
                    .take_log()
                    .into_iter()
                    .map(|line| format!("{:.4} MHz: {line}", m.frequency_hz as f64 / 1e6)),
            );
        }
        lines
    }

    /// Whether every multiplex's inputs are finite (see [`Station::inputs_finite`]).
    pub fn inputs_finite(&self) -> bool {
        !self.stations.is_empty() && self.stations.iter().all(Station::inputs_finite)
    }

    /// Whether every multiplex's inputs have ended (see [`Station::inputs_finished`]).
    pub fn inputs_finished(&self) -> bool {
        !self.stations.is_empty() && self.stations.iter().all(Station::inputs_finished)
    }

    fn update_status(&mut self) {
        let mut st = self.stations.first().map_or_else(StationStatus::default, |s| s.status().clone());
        st.frames = self.frames;
        st.seconds = self.frames as f64 * FRAME_SECONDS;
        st.output_file = None;
        st.device = None;
        st.device_underruns = 0;
        st.device_buffer_ms = None;
        // The 48 kHz stages of the multiplexes do not see the 8-bit conversion: the
        // sink counts the samples it had to clip.
        st.clipped_samples = self.sink.clipped_samples();
        st.multiplexes = self
            .group
            .multiplexes
            .iter()
            .zip(&self.stations)
            .map(|(m, station)| {
                let s = station.status();
                crate::station::MultiplexStatus {
                    frequency_hz: m.frequency_hz,
                    mode: format!("{} / {} kHz", m.plan.tx.mode, m.plan.tx.occupancy.bandwidth_khz()),
                    services: s.services.clone(),
                    frames: s.frames,
                    output_rms_dbfs: s.output_rms_dbfs,
                    sdc_blocks: s.sdc_blocks,
                    sdc_bytes_used: s.sdc_bytes_used,
                    sdc_capacity: s.sdc_capacity,
                }
            })
            .collect();
        let n = self.samples.len().max(1);
        let (sum, peak) = self
            .samples
            .iter()
            .fold((0.0f64, 0.0f32), |(s, p), &v| (s + f64::from(v * v), p.max(v.abs())));
        st.output_rms_dbfs = 20.0 * ((sum / n as f64).sqrt() as f32).max(1e-10).log10();
        st.output_peak_dbfs = 20.0 * peak.max(1e-10).log10();
        #[cfg(feature = "hackrf")]
        {
            let s = self.sink.settings();
            let half = self.group.signal_span_hz / 2.0;
            st.hackrf = Some(crate::hackrf::HackrfStatus {
                frequency_hz: s.frequency_hz,
                band_lo_hz: s.frequency_hz as f64 - half,
                band_hi_hz: s.frequency_hz as f64 + half,
                lo_hz: s.lo_hz(),
                sample_rate_hz: s.sample_rate_hz,
                if_offset_hz: s.if_offset_hz,
                tx_gain_db: s.tx_gain_db,
                amp: s.amp_enable,
                resampler: s.resampler,
                transmitted_s: self.sink.transmitted_s(),
                queued_s: self.sink.queued_s(),
                underruns: self.sink.underruns(),
                recoveries: self.sink.recoveries(),
            });
        }
        self.status = st;
    }
}

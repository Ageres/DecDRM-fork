//! `decdrm tx` — run the DRM transmitter described by a station configuration file.

use anyhow::{Context, Result, bail};
use decdrm_station::{GroupStation, ServiceStatus, Station, StationConfig, StationPlan, StationStatus, StopHandle};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

#[derive(clap::Args)]
pub struct TxArgs {
    /// Station configuration (TOML; see crates/decdrm-station/examples/station.toml).
    config: PathBuf,
    /// Seconds of signal to transmit. Default: until every (non-looping) input file has
    /// ended; without an end, sound-card output runs until interrupted.
    #[arg(long, value_name = "SECS")]
    duration: Option<f64>,
    /// Write the signal to this WAV/FLAC file instead of the configured outputs.
    #[arg(long, value_name = "FILE")]
    output: Option<PathBuf>,
    /// Print a status line every N seconds of signal (0 = never).
    #[arg(long, default_value_t = 5.0)]
    status_every: f64,
    /// Only check the configuration and print the multiplex.
    #[arg(long)]
    check: bool,
    /// Pass the signal through DRM channel model 1-6 (ES 201 980 annex B) before the
    /// outputs, overriding the file's [simulate] section.
    #[arg(long, value_name = "1-6")]
    channel_model: Option<u8>,
    /// Add white noise for this SNR (dB, in the nominal channel bandwidth); implies
    /// channel model 1 (AWGN) unless one is set.
    #[arg(long, value_name = "DB", allow_negative_numbers = true)]
    snr: Option<f64>,
}

/// Apply `--output`: write to `out` instead of the configured outputs. A group records
/// its composite instead of the radio, at the radio's rate when its section is there.
fn apply_output(cfg: &mut StationConfig, out: &Path) -> Result<()> {
    // Relative to the current directory, not to the configuration file.
    cfg.output.file = Some(std::path::absolute(out).with_context(|| format!("output path {}", out.display()))?);
    cfg.output.device = None;
    if !cfg.multiplexes.is_empty() {
        if cfg.output.sample_rate_hz.is_none() {
            cfg.output.sample_rate_hz = cfg.output.hackrf.as_ref().map(|h| h.sample_rate_hz);
        }
        cfg.output.hackrf = None;
    }
    Ok(())
}

pub fn run(a: TxArgs) -> Result<()> {
    let mut cfg = StationConfig::load(&a.config)?;
    if let Some(out) = &a.output {
        apply_output(&mut cfg, out)?;
    }
    if a.channel_model.is_some() || a.snr.is_some() {
        let sim = cfg.simulate.get_or_insert_with(|| decdrm_station::SimulateSettings::new(1, None));
        if let Some(model) = a.channel_model {
            sim.channel = model;
        }
        if a.snr.is_some() {
            sim.snr_db = a.snr;
        }
    }
    let plan = cfg.validate_station()?;
    print!("{}", plan.describe(&cfg));
    if a.check {
        return Ok(());
    }

    if let Some(d) = a.duration
        && !(d.is_finite() && d > 0.0)
    {
        bail!("--duration must be a positive number of seconds");
    }
    let frames = a.duration.map(|d| (d / 0.4).ceil() as u64);

    // Checked before the station creates its output file.
    if frames.is_none() && cfg.output.device.is_none() && cfg.output.hackrf.is_none() && !cfg.inputs_finite() {
        bail!("the signal goes to a file but has no end: give --duration SECS (or set `loop = false` on every audio input file)");
    }
    for url in cfg.services.iter().filter_map(|s| s.audio.as_ref()?.input.url.as_ref()) {
        println!("web stream: connecting to {url}");
    }
    // Creating the station waits for a web stream's first audio: let Ctrl-C end that.
    let stop = StopHandle::default();
    let created = Arc::new(AtomicBool::new(false));
    {
        let (stop, created) = (stop.clone(), created.clone());
        std::thread::spawn(move || {
            while !created.load(Ordering::Relaxed) {
                if crate::interrupted() {
                    stop.stop();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        });
    }
    let station = match plan {
        StationPlan::Single(plan) => Station::with_plan_and_stop(cfg, plan, stop).map(|s| Running::Single(Box::new(s))),
        StationPlan::Group(group) => GroupStation::with_stop(cfg, group, stop).map(|g| Running::Group(Box::new(g))),
    };
    created.store(true, Ordering::Relaxed);
    let mut station = station?;
    if let Some(dev) = &station.status().device {
        println!("sound card: {dev}");
    }
    if let Some(h) = &station.status().hackrf {
        println!(
            "hackrf: {:.4} MHz (LO {:.4} MHz + IF {:.0} kHz), {} Msps, TX gain {} dB{}, resampler {}",
            h.frequency_hz as f64 / 1e6,
            h.lo_hz as f64 / 1e6,
            h.if_offset_hz / 1e3,
            h.sample_rate_hz as f64 / 1e6,
            h.tx_gain_db,
            if h.amp { ", RF amp on" } else { "" },
            h.resampler
        );
        println!(
            "        occupied band: {:.1}..{:.1} kHz",
            h.band_lo_hz / 1e3,
            h.band_hi_hz / 1e3
        );
    }
    // The inputs' log: web stream connections, titles, reconnections.
    let print_log = |station: &mut Running| station.take_log().iter().for_each(|line| println!("{line}"));
    print_log(&mut station);
    let started = Instant::now();
    let mut next_status = a.status_every;
    let mut printed_at = None;
    loop {
        match frames {
            Some(n) if station.status().frames >= n => break,
            None if station.inputs_finished() => break,
            _ if crate::interrupted() => break,
            _ => {}
        }
        if let Err(e) = station.transmit_frame() {
            // Complete the file and hand the radio back before reporting.
            let _ = station.finish();
            return Err(e);
        }
        print_log(&mut station);
        let s = station.status();
        if a.status_every > 0.0 && s.seconds >= next_status {
            next_status += a.status_every;
            print_status(s);
            printed_at = Some(s.frames);
        }
    }
    print_log(&mut station);
    let s = station.finish()?;
    if printed_at != Some(s.frames) {
        print_status(&s);
    }
    println!("transmitted {:.1} s of signal in {:.2} s", s.seconds, started.elapsed().as_secs_f64());
    Ok(())
}

fn print_status(s: &StationStatus) {
    // A group's SDC is per multiplex: print it below, with the multiplexes.
    let sdc = if s.multiplexes.is_empty() {
        format!("; SDC {}/{} bytes", s.sdc_bytes_used, s.sdc_capacity)
    } else {
        String::new()
    };
    println!(
        "[{:6.1}s] output {:5.1} dBFS rms, {:5.1} dBFS peak, {} clipped{}{}{}",
        s.seconds,
        s.output_rms_dbfs,
        s.output_peak_dbfs,
        s.clipped_samples,
        sdc,
        s.time_sent.as_ref().map(|t| format!(", time {t}")).unwrap_or_default(),
        if s.device.is_some() { format!(", {} underruns", s.device_underruns) } else { String::new() }
    );
    for (i, m) in s.multiplexes.iter().enumerate() {
        println!(
            "          multiplex {}: {:.4} MHz, {}, {} service{}, {:.1} dBFS rms, {} frames, SDC {}/{} bytes",
            i + 1,
            m.frequency_hz as f64 / 1e6,
            m.mode,
            m.services.len(),
            if m.services.len() == 1 { "" } else { "s" },
            m.output_rms_dbfs,
            m.frames,
            m.sdc_bytes_used,
            m.sdc_capacity
        );
        for sv in &m.services {
            println!("{}", service_line(sv, false));
        }
    }
    if let Some(h) = &s.hackrf {
        let mut extra = String::new();
        if h.underruns > 0 {
            extra.push_str(&format!(", {} underruns", h.underruns));
        }
        if h.recoveries > 0 {
            extra.push_str(&format!(", {} recoveries", h.recoveries));
        }
        println!(
            "          hackrf: {:.1} s transmitted, {:.1} s buffered{extra}",
            h.transmitted_s, h.queued_s
        );
    }
    if let Some(m) = &s.mdi {
        let state = if m.waiting { "waiting for MDI".to_string() } else { m.channel.clone().unwrap_or_default() };
        println!(
            "          modulator: {} frames, {} fillers, {} dropped, {} queued; {state}; MDI from {}{}",
            m.frames,
            m.fillers,
            m.dropped,
            m.queued,
            m.input,
            m.sender.as_ref().map(|s| format!(" ({s})")).unwrap_or_default()
        );
    }
    if s.multiplexes.is_empty() {
        for sv in &s.services {
            println!("{}", service_line(sv, s.mdi.is_some()));
        }
    }
}

/// One detailed service line for [`print_status`] (a modulator's audio has no input).
fn service_line(sv: &ServiceStatus, mdi: bool) -> String {
    let mut line = format!("          service {} {:06X} \"{}\" {:.2} kbit/s", sv.short_id, sv.service_id, sv.label, sv.bitrate / 1000.0);
    if let Some(a) = &sv.audio
        && mdi
    {
        // A modulator's audio comes encoded with the MDI: no input, no levels.
        line.push_str(&format!(": {}", a.codec));
    } else if let Some(a) = &sv.audio {
        line.push_str(&format!(
            ": {}, input {:.1} dBFS{}{}{}{}",
            a.codec,
            a.counters.input_rms_dbfs,
            if a.input_finished { " (ended)" } else { "" },
            a.web_stream
                .as_ref()
                .map(|w| format!(
                    ", web stream {} ({:.1} s buffered){}",
                    w.state,
                    w.buffer_s,
                    w.title.as_ref().map(|t| format!(", \"{t}\"")).unwrap_or_default()
                ))
                .unwrap_or_default(),
            a.counters.input_drift_ppm.map(|p| format!(", clock trim {p:+.0} ppm")).unwrap_or_default(),
            if a.counters.frames_dropped > 0 { format!(", {} frames dropped", a.counters.frames_dropped) } else { String::new() }
        ));
    }
    for app in &sv.apps {
        line.push_str(&format!("; {} {:.2} kbit/s", app.kind, app.bitrate / 1000.0));
        if let Some(j) = &app.journaline {
            let source = if j.feeds.is_empty() { "page file" } else { "feeds" };
            let updates = if j.updates > 0 { format!(", {source} loaded again {}x", j.updates) } else { String::new() };
            let feeds = if j.feeds.is_empty() { String::new() } else { format!(" from {} feeds", j.feeds.len()) };
            line.push_str(&format!(" ({} pages{feeds}{updates})", j.pages));
        }
    }
    line
}

/// A running station: one multiplex, or a group of them through one radio.
enum Running {
    Single(Box<Station>),
    Group(Box<GroupStation>),
}

impl Running {
    fn transmit_frame(&mut self) -> Result<&[f32]> {
        Ok(match self {
            Self::Single(station) => station.transmit_frame()?,
            Self::Group(group) => group.transmit_frame()?,
        })
    }

    fn status(&self) -> &StationStatus {
        match self {
            Self::Single(station) => station.status(),
            Self::Group(group) => group.status(),
        }
    }

    fn take_log(&mut self) -> Vec<String> {
        match self {
            Self::Single(station) => station.take_log(),
            Self::Group(group) => group.take_log(),
        }
    }

    fn inputs_finished(&self) -> bool {
        match self {
            Self::Single(station) => station.inputs_finished(),
            Self::Group(group) => group.inputs_finished(),
        }
    }

    fn finish(self) -> Result<StationStatus> {
        Ok(match self {
            Self::Single(station) => station.finish()?,
            Self::Group(group) => group.finish()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GROUP: &str = r#"
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

    /// `--output` turns a radio group into a composite file at the radio's rate.
    #[test]
    fn a_group_records_its_composite_instead_of_the_radio() {
        let mut cfg = StationConfig::from_toml_str(GROUP).unwrap();
        apply_output(&mut cfg, Path::new("group.wav")).unwrap();
        assert!(cfg.output.hackrf.is_none(), "the file, not the radio");
        assert!(cfg.output.device.is_none());
        assert_eq!(cfg.output.sample_rate_hz, Some(2_000_000), "the radio's rate");
        assert!(cfg.output.file.as_deref().unwrap().to_string_lossy().ends_with("group.wav"));
        assert!(cfg.validate_station().is_ok());
    }
}
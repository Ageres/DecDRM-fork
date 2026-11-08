//! An end-to-end group: two multiplexes through [`GroupStation`] into one composite
//! file, checked at their offsets.

use decdrm_io::FileReader;
use decdrm_station::{GroupStation, StationConfig, StationPlan};

/// A small composite: 200 kHz I/Q, two 10 kHz multiplexes 20 kHz apart, tones for
/// audio (no network).
const TWO: &str = r#"
[output]
format = "iq"
file = "group.wav"
sample_rate_hz = 200_000
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

/// The composite's magnitude at `hz`, relative to a full-scale carrier.
fn magnitude_at(samples: &[f32], rate: f64, hz: f64) -> f64 {
    let step = std::f64::consts::TAU * hz / rate;
    let (mut re, mut im, mut phase) = (0.0f64, 0.0f64, 0.0f64);
    for &[i, q] in samples.as_chunks::<2>().0 {
        let (sin, cos) = phase.sin_cos();
        re += f64::from(i) * cos + f64::from(q) * sin;
        im += f64::from(q) * cos - f64::from(i) * sin;
        phase += step;
    }
    (re * re + im * im).sqrt() / (samples.len() as f64 / 2.0)
}

/// Mean power over `hz +- half_width`, at 500 Hz steps.
fn band_power(samples: &[f32], rate: f64, hz: f64, half_width: f64) -> f64 {
    let mut total = 0.0;
    let mut bins = 0;
    let mut f = hz - half_width;
    while f <= hz + half_width {
        total += magnitude_at(samples, rate, f).powi(2);
        bins += 1;
        f += 500.0;
    }
    total / f64::from(bins)
}

#[test]
fn two_multiplexes_write_one_composite() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = StationConfig::from_toml_str(TWO).unwrap();
    cfg.base_dir = Some(dir.path().to_path_buf());
    let StationPlan::Group(group) = cfg.validate_station().unwrap() else {
        panic!("expected a group");
    };
    assert_eq!(group.sample_rate_hz, 200_000);
    assert_eq!(group.if_offset_hz, 0.0, "a file has no radio IF");
    let rate = f64::from(group.sample_rate_hz);
    let offsets: Vec<f64> = group.multiplexes.iter().map(|m| m.offset_hz).collect();
    assert_eq!(offsets, vec![-10_000.0, 10_000.0]);

    let mut station = GroupStation::new(cfg, group).unwrap();
    // Nothing sent yet: the initial status does not invent a level.
    assert_eq!(station.status().output_rms_dbfs, 0.0);
    station.run_frames(3).unwrap();
    let status = station.finish().unwrap();
    assert_eq!(status.frames, 3);
    assert_eq!(status.multiplexes.len(), 2);
    let path = status.output_file.clone().expect("a composite file");
    assert!(path.exists());

    let mut reader = FileReader::open(&path).unwrap();
    assert_eq!(reader.format().sample_rate, 200_000);
    assert_eq!(reader.format().channels, 2);
    let samples = reader.read_all().unwrap();
    assert!(!samples.is_empty());

    // Each multiplex sits at its offset; the gap between them is quiet.
    let gap = band_power(&samples, rate, 0.0, 3_000.0);
    for offset in offsets {
        let channel = band_power(&samples, rate, offset, 4_000.0);
        assert!(channel > 5.0 * gap, "{offset} Hz: {channel} against the gap's {gap}");
    }
}
